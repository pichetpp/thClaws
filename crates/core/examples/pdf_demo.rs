//! Render a representative Thai/Latin markdown document through
//! PdfCreate for visual inspection:
//!     cargo run --example pdf_demo -- /tmp/pdf-demo.pdf
use thclaws_core::tools::{PdfCreateTool, Tool};

#[tokio::main]
async fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/pdf-demo.pdf".to_string());
    let md = r#"# บทที่ 1 — รู้จักผำ: พืชจิ๋วมหัศจรรย์แห่งแหล่งน้ำไทย

ผำ (วูล์ฟเฟีย) เป็น**พืชดอกที่เล็กที่สุดในโลก** ขนาดเม็ดเล็กกว่า 1 มิลลิเมตร ไม่มีราก ไม่มีใบที่แท้จริง ลอยอยู่บนผิวน้ำเป็นแพสีเขียวสด คนอีสานเรียก *ไข่น้ำ* หรือ *ไข่แหน* และนำมาประกอบอาหารมานานนับร้อยปี ทั้งแกงอ่อม ไข่เจียวผำ และแจ่วผำ

## คุณค่าทางโภชนาการ

งานวิจัยจากมหาวิทยาลัยมหิดลพบว่าผำแห้งมีโปรตีนสูงถึง **40%** ของน้ำหนักแห้ง เทียบกับแหล่งโปรตีนอื่นดังนี้:

| แหล่งโปรตีน | โปรตีน (%) | หมายเหตุ |
|---|---|---|
| ผำแห้ง | 40.5 | โปรตีนครบกรดอะมิโนจำเป็น |
| ไข่ไก่ | 12.6 | มาตรฐานเปรียบเทียบ |
| เนื้อหมู | 27.7 | ปรุงสุก |
| Spirulina | 57.5 | ราคาสูงกว่า 8 เท่า |

ข้อดีหลักของการเลี้ยงผำ:

1. โตเร็ว — เพิ่มมวลเป็น **2 เท่าภายใน 4 วัน**
2. ใช้พื้นที่น้อย เลี้ยงในกะละมังหลังบ้านได้
3. ต้นทุนต่ำ ใช้ปุ๋ยน้อยกว่าผักใบเขียวทั่วไป

> ผำคือ superfood ที่อยู่ใต้จมูกคนไทยมาตลอด — เพียงแต่โลกเพิ่งหันมามอง

### วิธีเริ่มต้นเลี้ยง

เตรียมน้ำสะอาดค่า pH ระหว่าง `6.5-7.5` แล้วทำตามขั้นตอน:

```
1. เตรียมบ่อ/กะละมัง ลึก 20-30 ซม.
2. เติมปุ๋ยสูตร 16-20-0 อัตรา 5 กรัม/น้ำ 100 ลิตร
3. โรยพันธุ์ผำ 100 กรัม/ตร.ม.
```

---

## English Section: Export Markets

The global demand for *plant-based protein* is growing at *11% CAGR*. Thai wolffia producers can target three segments: health-food retail, food-service ingredients, and **dried protein powder** for supplement manufacturers — provided farms meet GAP certification requirements.
"#;
    let result = PdfCreateTool
        .call(serde_json::json!({
            "path": out,
            "content": md,
            "title": "ไข่ผำ — sample chapter",
            "page_break_h1": true
        }))
        .await
        .unwrap();
    println!("{result}");
}
