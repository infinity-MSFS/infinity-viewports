#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ConfigSnapshot {
    pub generation: u64,
    pub enabled: u8,
    pub _pad: [u8; 7],
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandCode {
    Nop = 0,
    DumpCapture = 1,
    Rediscover = 2,
    Shutdown = 3,
    Teardown = 4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CommandSlot {
    pub code: u32,
    pub _pad: u32,
    pub arg: [u8; 240],
}

impl CommandSlot {
    pub const EMPTY: CommandSlot = CommandSlot {
        code: 0,
        _pad: 0,
        arg: [0; 240],
    };
}

pub const COMMAND_RING_SLOTS: usize = 16;
