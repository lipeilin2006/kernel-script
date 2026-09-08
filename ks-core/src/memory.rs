pub trait MemoryAccess {
    fn read_memory(&self, process_id: u64, address: u64, size: u64) -> Result<[u8; 256], u32>;
    fn write_memory(&self, process_id: u64, address: u64, data: &[u8]) -> Result<(), u32>;
}

pub fn calculate_physical_address(virtual_address: u64, cr3: u64) -> u64 {
    let pd_index = (virtual_address >> 39) & 0x1FF;
    let pt_index = (virtual_address >> 30) & 0x1FF;
    let page_index = (virtual_address >> 12) & 0x1FF;
    let offset = virtual_address & 0xFFF;

    let pd_entry = cr3 + pd_index * 8;
    let pt_entry = pd_entry + pt_index * 8;
    let page_entry = pt_entry + page_index * 8;

    (page_entry & 0xFFFFFFFFFFFFF000) | offset
}
