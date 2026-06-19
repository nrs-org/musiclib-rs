//! Minimal, dependency-free FlatBuffer reader for the TFLite model container.
//! We only need to walk `Model.buffers : [Buffer]` and pull out each
//! `Buffer.data : [ubyte]` blob; everything else in the schema is ignored.
//!
//! FlatBuffer layout recap (little-endian):
//!   * file root: u32 uoffset to the root table.
//!   * table: i32 soffset to its vtable at `table - soffset`; fields are read
//!     through the vtable, which stores u16 byte-offsets (relative to the table
//!     start) per field, 0 meaning "absent".
//!   * vtable: [u16 vtable_bytes][u16 table_bytes][u16 field0]...[u16 fieldN].
//!   * vector/string/table fields store a u32 uoffset (relative to the field
//!     slot) to the actual data; scalar fields are stored inline.
//!   * vector: [u32 len][elements...].

struct Fb<'a> {
    buf: &'a [u8],
}

impl<'a> Fb<'a> {
    fn u32(&self, pos: usize) -> usize {
        u32::from_le_bytes(self.buf[pos..pos + 4].try_into().unwrap()) as usize
    }
    fn i32(&self, pos: usize) -> i32 {
        i32::from_le_bytes(self.buf[pos..pos + 4].try_into().unwrap())
    }
    fn u16(&self, pos: usize) -> usize {
        u16::from_le_bytes(self.buf[pos..pos + 2].try_into().unwrap()) as usize
    }

    /// Byte position of a table field's value, or `None` if the field is absent.
    fn field(&self, table: usize, field_id: usize) -> Option<usize> {
        let soffset = self.i32(table);
        let vtable = (table as i64 - soffset as i64) as usize;
        let vtable_bytes = self.u16(vtable);
        let voff = 4 + 2 * field_id;
        if voff >= vtable_bytes {
            return None;
        }
        let foff = self.u16(vtable + voff);
        if foff == 0 {
            return None;
        }
        Some(table + foff)
    }

    /// Follow a uoffset field to a vector; returns (elements_start, len).
    fn vector(&self, field_pos: usize) -> (usize, usize) {
        let vpos = field_pos + self.u32(field_pos);
        let len = self.u32(vpos);
        (vpos + 4, len)
    }
}

/// Extract `Model.buffers` as raw byte slices indexed by buffer index.
pub fn buffers(model: &[u8]) -> Vec<&[u8]> {
    let fb = Fb { buf: model };
    let root = fb.u32(0); // Model table position
    // Schema field order: version=0, operator_codes=1, subgraphs=2,
    // description=3, buffers=4, ...
    let buffers_field = fb.field(root, 4).expect("Model.buffers missing");
    let (start, len) = fb.vector(buffers_field);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let elem = start + i * 4;
        let table = elem + fb.u32(elem); // Buffer table position
        match fb.field(table, 0) {
            Some(data_field) => {
                let (dstart, dlen) = fb.vector(data_field);
                out.push(&model[dstart..dstart + dlen]);
            }
            None => out.push(&model[0..0]),
        }
    }
    out
}
