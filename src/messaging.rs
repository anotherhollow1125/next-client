use std::convert::{Into, TryFrom, TryInto};

pub struct NCSyncMessage {
    pub kind: NCSyncKind,
    pub is_recursive: bool,
    pub target: String,
}

#[derive(Clone, Copy)]
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
        if value.len() < 2 {
            return Err(anyhow!("Invalid array."));
        }

        let kind = value[0].try_into()?;
        let is_recursive = match value[1] {
            0 => false,
            _ => true,
        };
        let target = String::from_utf8((&value[2..]).to_vec())?;

        Ok(Self {
            kind,
            is_recursive,
            target,
        })
    }
}

impl Into<Vec<u8>> for NCSyncMessage {
    fn into(self) -> Vec<u8> {
        let mut res = Vec::with_capacity(2);
        res.push(self.kind.into());
        res.push(self.is_recursive.into());
        res.extend_from_slice(self.target.as_bytes());

        res
    }
}
