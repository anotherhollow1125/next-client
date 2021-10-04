use std::convert::{Into, TryFrom, TryInto};

#[derive(Debug)]
pub struct NCSyncMessage {
    pub kind: NCSyncKind,
    pub is_recursive: bool,
    pub use_stash: bool,
    pub target: String,
}

#[derive(Debug, Clone, Copy)]
pub enum NCSyncKind {
    Push,
    Pull,
}

impl TryFrom<u8> for NCSyncKind {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Push),
            1 => Ok(Self::Pull),
            _ => Err(anyhow!("Invalid kind")),
        }
    }
}

impl Into<u8> for NCSyncKind {
    fn into(self) -> u8 {
        match self {
            Self::Push => 0,
            Self::Pull => 1,
        }
    }
}

impl TryFrom<&[u8]> for NCSyncMessage {
    type Error = anyhow::Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() < 3 {
            return Err(anyhow!("Invalid array."));
        }

        let kind = value[0].try_into()?;
        let is_recursive = match value[1] {
            0 => false,
            _ => true,
        };
        let use_stash = match value[2] {
            0 => false,
            _ => true,
        };
        let target = String::from_utf8((&value[3..]).to_vec())?;

        Ok(Self {
            kind,
            is_recursive,
            use_stash,
            target,
        })
    }
}

impl Into<Vec<u8>> for NCSyncMessage {
    fn into(self) -> Vec<u8> {
        let mut res = Vec::with_capacity(3);
        res.push(self.kind.into());
        res.push(self.is_recursive.into());
        res.push(self.use_stash.into());
        res.extend_from_slice(self.target.as_bytes());

        res
    }
}
