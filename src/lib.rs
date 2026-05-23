/// Indexing and search logic (Bloom filters, BlockIndex).
pub mod index;
/// File locking mechanism for concurrency safety.
pub mod lock;
/// Data schemas and serialization (DTOs, Events).
pub mod schema;
/// Storage engine core (WAL, Append/Rotate, Compaction).
pub mod storage;

#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
mod aarch64_windows_stubs {
    #[unsafe(no_mangle)]
    pub extern "C" fn __chkstk() {}

    // Shift-subtract division algorithm for u128 to avoid native / and % operators.
    // Native / and % operators on u128 compile into recursive calls of __udivti3 / __umodti3.
    fn udivmod128(num: u128, den: u128) -> (u128, u128) {
        if den == 0 {
            return (0, 0);
        }
        if num < den {
            return (0, num);
        }
        let mut quot = 0u128;
        let mut rem = 0u128;
        for i in (0..128).rev() {
            rem = (rem << 1) | ((num >> i) & 1);
            if rem >= den {
                rem -= den;
                quot |= 1 << i;
            }
        }
        (quot, rem)
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn __udivti3(n: u128, d: u128) -> u128 {
        udivmod128(n, d).0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn __umodti3(n: u128, d: u128) -> u128 {
        udivmod128(n, d).1
    }
}

