use super::{CpuTimes, Hardware, Memory, Volume};

pub fn hardware() -> Hardware {
    Hardware {
        cpu: std::env::consts::ARCH.into(),
        notes: vec!["Hardware monitoring is unavailable on this OS".into()],
        ..Default::default()
    }
}

pub fn cpu_times() -> Option<CpuTimes> {
    None
}

pub fn memory() -> Option<Memory> {
    None
}

pub fn volumes() -> Vec<Volume> {
    Vec::new()
}
