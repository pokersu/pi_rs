//! 对应 `src/ids.ts`：在可信边界上施加被擦除的品牌。
//!
//! Rust 的 [`Id`] / [`Seq`] 已是强类型，这里只保留上游的同名入口以便对照。

use crate::types::{Id, Seq};

/// 对应 `idFromNumber`：在可信的数值分配或解码边界施加 ID 品牌。
pub fn id_from_number<I: IdBrand>(value: u64) -> I {
    I::from_number(value)
}

/// 对应 `seqFromNumber`：在可信的存储边界施加提交序号品牌。
pub const fn seq_from_number(value: u64) -> Seq {
    Seq::new(value)
}

/// 让 [`id_from_number`] 能对任意 `Id<Kind, T>` 泛化。
pub trait IdBrand {
    /// 由原始数值构造。
    fn from_number(value: u64) -> Self;
}

impl<Kind, T> IdBrand for Id<Kind, T> {
    fn from_number(value: u64) -> Self {
        Id::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConversationId, EntryId};

    #[test]
    fn id_from_number_applies_the_requested_brand() {
        let conversation: ConversationId = id_from_number(3);
        let entry: EntryId = id_from_number(3);
        assert_eq!(conversation.get(), 3);
        assert_eq!(entry.get(), 3);
    }

    #[test]
    fn seq_from_number_wraps_value() {
        assert_eq!(seq_from_number(9).get(), 9);
    }
}
