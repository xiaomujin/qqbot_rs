use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 事件订阅位标记。
///
/// 手写位运算而非引入 `bitflags`，减少依赖并保持 `const` 可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Intents(u32);

impl Intents {
    pub const fn empty() -> Self {
        Intents(0)
    }

    pub const fn from_bits(bits: u32) -> Self {
        Intents(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Intents(self.0 | other.0)
    }

    pub const GUILDS: Self = Intents(1 << 0);
    pub const GUILD_MEMBERS: Self = Intents(1 << 1);
    /// 仅私域机器人可设置。
    pub const GUILD_MESSAGES: Self = Intents(1 << 9);
    pub const GUILD_MESSAGE_REACTIONS: Self = Intents(1 << 10);
    pub const DIRECT_MESSAGE: Self = Intents(1 << 12);
    /// 单聊 + 群聊的全部事件（C2C_MESSAGE_CREATE / GROUP_AT_MESSAGE_CREATE / ...）。
    pub const GROUP_AND_C2C_EVENT: Self = Intents(1 << 25);
    /// 按钮等互动事件。
    pub const INTERACTION: Self = Intents(1 << 26);
    pub const MESSAGE_AUDIT: Self = Intents(1 << 27);
    /// 仅私域机器人可设置。
    pub const FORUMS_EVENT: Self = Intents(1 << 28);
    pub const AUDIO_ACTION: Self = Intents(1 << 29);
}

impl std::ops::BitOr for Intents {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for Intents {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl Serialize for Intents {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for Intents {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Intents(u32::deserialize(d)?))
    }
}

impl std::fmt::Display for Intents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Intents(0x{:08X})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_and_c2c_bit_is_25() {
        assert_eq!(Intents::GROUP_AND_C2C_EVENT.bits(), 1 << 25);
        assert_eq!(Intents::GROUP_AND_C2C_EVENT.bits(), 33_554_432);
    }

    #[test]
    fn union_and_contains() {
        let i = Intents::GROUP_AND_C2C_EVENT | Intents::INTERACTION;
        assert!(i.contains(Intents::GROUP_AND_C2C_EVENT));
        assert!(i.contains(Intents::INTERACTION));
        assert!(!i.contains(Intents::GUILDS));
    }
}
