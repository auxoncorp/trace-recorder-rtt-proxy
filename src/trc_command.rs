/// Trace recorder control-plane commands
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, derive_more::Display)]
pub enum TrcCommand {
    /// Start tracing.
    ///
    /// CMD_SET_ACTIVE with param1 set to 1.
    #[display("start-tracing")]
    StartTracing,

    /// Stop tracing.
    ///
    /// CMD_SET_ACTIVE with param1 set to 0.
    #[display("stop-tracing")]
    StopTracing,
}

impl TrcCommand {
    /// Wire size of a command, equivalent to `sizeof(TracealyzerCommandType)`
    pub const WIRE_SIZE: usize = 8;

    /// param1 = 1 means start, param1 = 0 means stop
    const CMD_SET_ACTIVE: u8 = 1;

    fn param0(&self) -> u8 {
        match self {
            TrcCommand::StartTracing | TrcCommand::StopTracing => Self::CMD_SET_ACTIVE,
        }
    }

    fn param1(&self) -> u8 {
        match self {
            TrcCommand::StartTracing => 1,
            TrcCommand::StopTracing => 0,
        }
    }

    fn checksum(&self) -> u16 {
        let sum = u16::from(self.param0())
                // param2..=param5 are always zero
                + u16::from(self.param1());
        0xFFFF_u16.wrapping_sub(sum)
    }

    /// Convert a command to its `TracealyzerCommandType` wire representation
    pub fn to_wire_bytes(self) -> [u8; Self::WIRE_SIZE] {
        let checksum_bytes = self.checksum().to_le_bytes();
        [
            self.param0(),
            self.param1(),
            0,
            0,
            0,
            0,
            checksum_bytes[0],
            checksum_bytes[1],
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_bytes() {
        let bytes = TrcCommand::StartTracing.to_wire_bytes();
        assert_eq!(bytes, [0x01, 0x01, 0, 0, 0, 0, 0xFD, 0xFF]);
        let bytes = TrcCommand::StopTracing.to_wire_bytes();
        assert_eq!(bytes, [0x01, 0x00, 0, 0, 0, 0, 0xFE, 0xFF]);
    }
}
