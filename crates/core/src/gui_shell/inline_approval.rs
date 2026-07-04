//! Inline tool-approval hub for GUI shells (dev-plan/39 Tier 3).
//!
//! By default a shell's `thclaws.tools.invoke` / `callTool` for a
//! mutating tool routes through the [`GuiApprover`], which pops the
//! full-screen system approval modal over the shell — jarring for a
//! shell that has its own diff/approve UI. When the shell has an inline
//! approval handler (it called `thclaws.approvals.subscribe`, so the
//! bridge sends `preferInline: true` on the invoke), the IPC handler
//! instead:
//!   1. [`register`]s a pending decision keyed by a fresh id,
//!   2. dispatches an `approval_request` `gui_shell_event` to the shell,
//!   3. awaits the decision — the shell renders its own widget and calls
//!      `thclaws.approvals.respond(id, decision)`, which lands on the
//!      `gui_shell_approval_respond` IPC command → [`resolve`].
//!
//! Self-contained (a process-global map), so it doesn't thread a new
//! field through `IpcContext`. Scope is deliberately the shell's OWN
//! tool invocations — the agent-turn approval path is unchanged.
//!
//! [`GuiApprover`]: crate::permissions::GuiApprover

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::oneshot;

use crate::permissions::ApprovalDecision;

fn pending() -> &'static Mutex<HashMap<u64, oneshot::Sender<ApprovalDecision>>> {
    static P: OnceLock<Mutex<HashMap<u64, oneshot::Sender<ApprovalDecision>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Register a pending inline approval. Returns the id to send to the
/// shell and the receiver to await the shell's decision.
pub fn register() -> (u64, oneshot::Receiver<ApprovalDecision>) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    if let Ok(mut p) = pending().lock() {
        p.insert(id, tx);
    }
    (id, rx)
}

/// Resolve a pending inline approval (called from the
/// `gui_shell_approval_respond` IPC command). No-op if the id is
/// unknown (already resolved / timed out).
pub fn resolve(id: u64, decision: ApprovalDecision) {
    let responder = pending().lock().ok().and_then(|mut p| p.remove(&id));
    if let Some(tx) = responder {
        let _ = tx.send(decision);
    }
}

/// Drop a pending entry without resolving (timeout / disconnect cleanup).
pub fn forget(id: u64) {
    if let Ok(mut p) = pending().lock() {
        p.remove(&id);
    }
}

/// Parse the wire decision string the bridge sends. Unknown → `Deny`
/// (fail-closed: a malformed response never silently approves).
pub fn parse_decision(s: &str) -> ApprovalDecision {
    match s {
        "allow" => ApprovalDecision::Allow,
        "allow_for_session" => ApprovalDecision::AllowForSession,
        _ => ApprovalDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_resolve_delivers_decision() {
        let (id, rx) = register();
        resolve(id, ApprovalDecision::Allow);
        assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn ids_are_unique() {
        let (a, _ra) = register();
        let (b, _rb) = register();
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_unknown_id_is_noop() {
        resolve(999_999, ApprovalDecision::Allow); // must not panic
    }

    #[tokio::test]
    async fn forget_drops_the_responder() {
        let (id, rx) = register();
        forget(id);
        // Sender dropped → receiver errors (caller treats as Deny).
        assert!(rx.await.is_err());
    }

    #[test]
    fn parse_decision_fails_closed() {
        assert_eq!(parse_decision("allow"), ApprovalDecision::Allow);
        assert_eq!(
            parse_decision("allow_for_session"),
            ApprovalDecision::AllowForSession
        );
        assert_eq!(parse_decision("deny"), ApprovalDecision::Deny);
        assert_eq!(parse_decision("garbage"), ApprovalDecision::Deny);
        assert_eq!(parse_decision(""), ApprovalDecision::Deny);
    }
}
