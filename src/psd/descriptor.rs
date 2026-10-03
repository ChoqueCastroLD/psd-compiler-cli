use super::reader::Reader;
use crate::error::{bail, Result};

/// A value inside an action descriptor.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Descriptor(Descriptor),
    List(Vec<Value>),
    Number(f64),
    Unit(String, f64),
    Text(String),
    Enum(String, String),
    Integer(i64),
    Bool(bool),
    Class(String),
    Raw(Vec<u8>),
    Reference,
}

/// Photoshop's generic key/value structure used by text, warp and effect blocks.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Descriptor {
    pub class: String,
    pub items: Vec<(String, Value)>,
}

impl Descriptor {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.items.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Value::Number(v) | Value::Unit(_, v) => Some(*v),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn desc(&self, key: &str) -> Option<&Descriptor> {
        match self.get(key)? {
            Value::Descriptor(d) => Some(d),
            _ => None,
        }
    }

    pub fn enumerated(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Enum(_, e) => Some(e),
            _ => None,
        }
    }

    pub fn list(&self, key: &str) -> Option<&[Value]> {
        match self.get(key)? {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn raw(&self, key: &str) -> Option<&[u8]> {
        match self.get(key)? {
            Value::Raw(r) => Some(r),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// Reads a descriptor that starts after `skip` bytes of `block`.
pub(crate) fn parse_block(block: &[u8], skip: usize) -> Result<Descriptor> {
    read(&mut Reader::at(block, skip, false))
}

fn key(r: &mut Reader) -> Result<String> {
    let n = r.u32()? as usize;
    Ok(String::from_utf8_lossy(r.bytes(if n == 0 { 4 } else { n })?).into_owned())
}

pub(crate) fn read(r: &mut Reader) -> Result<Descriptor> {
    r.unicode()?;
    let class = key(r)?;
    let count = r.u32()? as usize;
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let k = key(r)?;
        let t = r.tag()?;
        items.push((k, value(r, &t)?));
    }
    Ok(Descriptor { class, items })
}

fn unit_name(tag: [u8; 4]) -> String {
    String::from_utf8_lossy(&tag).into_owned()
}

fn value(r: &mut Reader, tag: &[u8; 4]) -> Result<Value> {
    Ok(match tag {
        b"Objc" | b"GlbO" => Value::Descriptor(read(r)?),
        b"VlLs" => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let t = r.tag()?;
                v.push(value(r, &t)?);
            }
            Value::List(v)
        }
        b"doub" => Value::Number(r.f64()?),
        b"UntF" => {
            let unit = unit_name(r.tag()?);
            Value::Unit(unit, r.f64()?)
        }
        b"UnFl" => {
            let unit = unit_name(r.tag()?);
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                v.push(Value::Unit(unit.clone(), r.f64()?));
            }
            Value::List(v)
        }
        b"TEXT" => Value::Text(r.unicode()?),
        b"enum" => {
            let ty = key(r)?;
            Value::Enum(ty, key(r)?)
        }
        b"long" => Value::Integer(r.i32()? as i64),
        b"comp" => Value::Integer(r.u64()? as i64),
        b"bool" => Value::Bool(r.u8()? != 0),
        b"type" | b"GlbC" => {
            r.unicode()?;
            Value::Class(key(r)?)
        }
        b"alis" | b"tdta" | b"Pth " => {
            let n = r.u32()? as usize;
            Value::Raw(r.bytes(n)?.to_vec())
        }
        b"ObAr" => {
            r.u32()?;
            r.unicode()?;
            key(r)?;
            let n = r.u32()? as usize;
            let mut d = Descriptor::default();
            for _ in 0..n {
                let k = key(r)?;
                let t = r.tag()?;
                d.items.push((k, value(r, &t)?));
            }
            Value::Descriptor(d)
        }
        b"obj " => {
            let n = r.u32()? as usize;
            for _ in 0..n {
                match &r.tag()? {
                    b"prop" => {
                        r.unicode()?;
                        key(r)?;
                        key(r)?;
                    }
                    b"Clss" => {
                        r.unicode()?;
                        key(r)?;
                    }
                    b"Enmr" => {
                        r.unicode()?;
                        key(r)?;
                        key(r)?;
                        key(r)?;
                    }
                    b"rele" => {
                        r.unicode()?;
                        key(r)?;
                        r.u32()?;
                    }
                    b"Idnt" | b"indx" => {
                        r.u32()?;
                    }
                    b"name" => {
                        r.unicode()?;
                        key(r)?;
                        r.unicode()?;
                    }
                    other => bail!("unknown reference type {:?}", String::from_utf8_lossy(other)),
                }
            }
            Value::Reference
        }
        _ => bail!("unknown descriptor type {:?}", String::from_utf8_lossy(tag)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct W(Vec<u8>);

    impl W {
        fn u32(&mut self, v: u32) -> &mut Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn raw(&mut self, b: &[u8]) -> &mut Self {
            self.0.extend_from_slice(b);
            self
        }
        fn key(&mut self, k: &str) -> &mut Self {
            if k.len() == 4 {
                self.u32(0).raw(k.as_bytes())
            } else {
                self.u32(k.len() as u32).raw(k.as_bytes())
            }
        }
        fn unicode(&mut self, s: &str) -> &mut Self {
            let units: Vec<u16> = s.encode_utf16().collect();
            self.u32(units.len() as u32);
            for u in units {
                self.raw(&u.to_be_bytes());
            }
            self
        }
    }

    #[test]
    fn reads_nested_values() {
        let mut w = W(vec![]);
        w.unicode("").key("null").u32(6);
        w.key("Scl ").raw(b"UntF").raw(b"#Prc").raw(&50.0f64.to_be_bytes());
        w.key("enab").raw(b"bool").raw(&[1]);
        w.key("Md  ").raw(b"enum").key("BlnM").key("Mltp");
        w.key("Txt ").raw(b"TEXT").unicode("héllo");
        w.key("Clr ").raw(b"Objc").unicode("").key("RGBC").u32(1);
        w.key("Rd  ").raw(b"doub").raw(&255.0f64.to_be_bytes());
        w.key("list").raw(b"VlLs").u32(2).raw(b"long").u32(7).raw(b"long").u32(9);
        let d = read(&mut Reader::new(&w.0)).unwrap();
        assert_eq!(d.class, "null");
        assert_eq!(d.num("Scl "), Some(50.0));
        assert_eq!(d.bool("enab"), Some(true));
        assert_eq!(d.enumerated("Md  "), Some("Mltp"));
        assert_eq!(d.text("Txt "), Some("héllo"));
        assert_eq!(d.desc("Clr ").and_then(|c| c.num("Rd  ")), Some(255.0));
        assert_eq!(d.list("list").map(|l| l.len()), Some(2));
        assert_eq!(d.num("missing"), None);
        assert_eq!(d.bool("Scl "), None);
    }

    #[test]
    fn long_keys_use_explicit_length() {
        let mut w = W(vec![]);
        w.unicode("").key("null").u32(1).key("masterFXSwitch").raw(b"bool").raw(&[0]);
        assert_eq!(read(&mut Reader::new(&w.0)).unwrap().bool("masterFXSwitch"), Some(false));
    }

    #[test]
    fn rejects_unknown_types() {
        let mut w = W(vec![]);
        w.unicode("").key("null").u32(1).key("abcd").raw(b"????");
        assert!(read(&mut Reader::new(&w.0)).is_err());
    }
}
