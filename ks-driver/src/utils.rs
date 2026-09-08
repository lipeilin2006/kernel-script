pub fn find_process_by_name(name: &str) -> Result<u64, u32> {
    crate::memory::process::find_process_by_name(name)
}

pub fn get_system_info() -> SystemInfo {
    SystemInfo {
        number_of_processors: get_number_of_processors(),
        page_size: get_page_size(),
        processor_type: get_processor_type(),
    }
}

fn get_number_of_processors() -> u32 {
    // In real implementation, this would call KeQueryActiveProcessorCount
    1
}

fn get_page_size() -> u32 {
    // In real implementation, this would call MmQueryPageSize
    4096
}

fn get_processor_type() -> u32 {
    // In real implementation, this would call KeQueryProcessorType
    0
}

pub struct SystemInfo {
    pub number_of_processors: u32,
    pub page_size: u32,
    pub processor_type: u32,
}

pub fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;

    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
        );
    }

    ((high as u64) << 32) | (low as u64)
}

pub fn write_msr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;

    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
        );
    }
}

pub fn get_cr0() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr0", out(reg) value);
    }
    value
}

pub fn set_cr0(value: u64) {
    unsafe {
        core::arch::asm!("mov cr0, {}", in(reg) value);
    }
}

pub fn get_cr4() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) value);
    }
    value
}

pub fn set_cr4(value: u64) {
    unsafe {
        core::arch::asm!("mov cr4, {}", in(reg) value);
    }
}

pub fn disable_write_protection() {
    let cr0 = get_cr0();
    set_cr0(cr0 & !0x10000);
}

pub fn enable_write_protection() {
    let cr0 = get_cr0();
    set_cr0(cr0 | 0x10000);
}
