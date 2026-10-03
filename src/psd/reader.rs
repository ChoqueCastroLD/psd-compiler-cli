use crate::error::{bail, Result};

/// Big-endian cursor over PSD bytes. `psb` widens the length fields that grow in large-document files.
pub(crate) struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
    pub psb: bool,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0, psb: false }
    }

    pub fn at(data: &'a [u8], pos: usize, psb: bool) -> Self {
        Reader { data, pos, psb }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(end) = self.pos.checked_add(n).filter(|&e| e <= self.data.len()) else {
            bail!("unexpected end of data at byte {} (wanted {n} more)", self.pos)
        };
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.array()?))
    }

    pub fn tag(&mut self) -> Result<[u8; 4]> {
        self.array()
    }

    /// Section length: 4 bytes in PSD, 8 bytes in PSB.
    pub fn length(&mut self) -> Result<usize> {
        Ok(if self.psb { self.u64()? as usize } else { self.u32()? as usize })
    }

    /// UTF-16BE string prefixed by its length in code units.
    pub fn unicode(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        let units: Vec<u16> =
            self.bytes(n.saturating_mul(2))?.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        Ok(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_big_endian_integers() {
        let data = [0x12, 0x34, 0xff, 0xfe, 0, 0, 0, 7];
        let mut r = Reader::new(&data);
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.i16().unwrap(), -2);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn reads_unicode_and_strips_trailing_nul() {
        let data = [0, 0, 0, 3, 0, b'h', 0, b'i', 0, 0];
        assert_eq!(Reader::new(&data).unicode().unwrap(), "hi");
    }

    #[test]
    fn length_width_depends_on_psb() {
        let data = [0, 0, 0, 0, 0, 0, 0, 9];
        assert_eq!(Reader::at(&data, 0, false).length().unwrap(), 0);
        assert_eq!(Reader::at(&data, 0, true).length().unwrap(), 9);
    }

    #[test]
    fn reports_truncation_instead_of_panicking() {
        let mut r = Reader::new(&[1, 2]);
        assert!(r.u32().is_err());
        assert!(r.bytes(usize::MAX).is_err());
    }
}
