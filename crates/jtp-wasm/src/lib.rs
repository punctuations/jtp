use xxhash_rust::xxh64::xxh64;

const IMAGE_ID_LEN: usize = 8;
const IMAGE_ID_HEX_LEN: usize = IMAGE_ID_LEN * 2;

#[no_mangle]
pub extern "C" fn image_id_hex_len() -> usize {
    IMAGE_ID_HEX_LEN
}

#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[no_mangle]
pub extern "C" fn dealloc(ptr: *mut u8, cap: usize) {
    if ptr.is_null() || cap == 0 {
        return;
    }
    unsafe {
        let _ = Vec::from_raw_parts(ptr, 0, cap);
    }
}

#[no_mangle]
pub extern "C" fn image_id_hex(input_ptr: *const u8, input_len: usize) -> *mut u8 {
    if input_ptr.is_null() {
        return std::ptr::null_mut();
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let id = xxh64(input, 0);
    let id_bytes = id.to_be_bytes();

    let mut out = [0u8; IMAGE_ID_HEX_LEN];
    hex_encode_8(&id_bytes, &mut out);

    let mut vec = Vec::with_capacity(IMAGE_ID_HEX_LEN);
    vec.extend_from_slice(&out);

    let ptr = vec.as_mut_ptr();
    let cap = vec.capacity();
    debug_assert!(cap >= IMAGE_ID_HEX_LEN);
    std::mem::forget(vec);

    ptr
}

fn hex_encode_8(bytes: &[u8], out: &mut [u8; IMAGE_ID_HEX_LEN]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
}
