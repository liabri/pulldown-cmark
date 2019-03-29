//! SIMD byte scanning logic.
//! Inspired by this great overview: http://0x80.pl/articles/simd-byte-lookup.html
//! However, since all of the bytes we're interested in, we don't quite need the
//! fully generality of the universal algorithm and are hence able to skip a few
//! instructions.

#[cfg(all(target_arch = "x86_64", feature="simd"))]
use core::arch::x86_64::*;

pub(crate) enum LoopInstruction<T> {
    /// Continue looking for more special bytes, but skip next few bytes.
    ContinueAndSkip(usize),
    /// Break looping immediately, returning with the given index and value.
    BreakAtWith(usize, T)
}

#[allow(overflowing_literals)]
#[cfg(all(target_arch = "x86_64", feature="simd"))]
unsafe fn compute_mask(bytes: &[u8], ix: isize) -> i32 {
    // constants. computed with the below code
    // ```rust
    // let chars = [b'\n', b'\r', b'*', b'_', b'~', b'|', b'&', b'\\', b'[', b']', b'<', b'!', b'`'];
    // let mut lower_bitmap = [0u8; 16];
    // for &c in &chars {
    //     lower_bitmap[(c & 0x0f) as usize] |= 1 << (c >> 4);
    // }
    // let bitmap = unsafe { _mm_loadu_si128(lower_bitmap.as_ptr() as *const _) };
    // ```
    let bitmap = _mm_setr_epi8(
        64, 4, 0, 0, 0, 0, 4, 0, 0, 0, 5, 32, 168, 33, 128, 32
    );
    let bitmask_lookup = _mm_setr_epi8(
        1, 2, 4, 8, 16, 32, 64, -128,
        255, 255, 255, 255, 255, 255, 255, 255
    );

    // actual computation
    let raw_ptr = bytes.as_ptr().offset(ix) as *const _;
    let input = _mm_loadu_si128(raw_ptr);
    let bitset = _mm_shuffle_epi8(bitmap, input);
    let higher_nibbles = _mm_and_si128(_mm_srli_epi16(input, 4), _mm_set1_epi8(0x0f));
    let bitmask = _mm_shuffle_epi8(bitmask_lookup, higher_nibbles);
    let tmp = _mm_and_si128(bitset, bitmask);
    let result = _mm_cmpeq_epi8(tmp, bitmask);
    _mm_movemask_epi8(result)
}

/// Calls callback on byte indices and their type.
/// Breaks when callback returns LoopInstruction::BreakAtWith(ix, val). And skips the
/// number of bytes in callback return value otherwise.
/// This method returns final index and a possible break value.
#[cfg(all(target_arch = "x86_64", feature="simd"))]
pub(crate) fn iterate_special_bytes<F, T>(bytes: &[u8], ix: usize, callback: F) -> (usize, Option<T>)
    where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    if is_x86_feature_detected!("ssse3") && bytes.len() >= 16 {
        unsafe {
            simd_iterate_special_bytes(bytes, ix, callback)
        }
    } else {
        scalar_iterate_special_bytes(bytes, ix, callback)
    }
}

#[cfg(not(all(target_arch = "x86_64", feature="simd")))]
pub(crate) fn iterate_special_bytes<F, T>(bytes: &[u8], ix: usize, callback: F) -> (usize, Option<T>)
    where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    scalar_iterate_special_bytes(bytes, ix, callback)
}

// Returns Ok to continue, Err to break
#[cfg(all(target_arch = "x86_64", feature="simd"))]
unsafe fn process_mask<F, T>(mut mask: i32, bytes: &[u8], mut ix: usize, callback: &mut F)
    -> Result<usize, (usize, Option<T>)>
where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    while mask != 0 {
        let mask_ix = mask.trailing_zeros() as usize;
        ix += mask_ix;
        match callback(ix, *bytes.get_unchecked(ix)) {
            LoopInstruction::ContinueAndSkip(skip) => {
                ix += skip + 1;
                mask >>= skip + 1 + mask_ix;
            }
            LoopInstruction::BreakAtWith(ix, val) => return Err((ix, val)),
        }
    }
    Ok(ix)
}

#[cfg(all(target_arch = "x86_64", feature="simd"))]
#[target_feature(enable = "ssse3")]
/// Important: only call this function when `bytes.len() >= 16`. Doing
/// so otherwise may exhibit undefined behaviour.
unsafe fn simd_iterate_special_bytes<F, T>(bytes: &[u8], mut ix: usize, mut callback: F) -> (usize, Option<T>)
    where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    let upperbound = bytes.len() - 16;
    
    while ix < upperbound {
        let mask = compute_mask(bytes, ix as isize);
        let block_start = ix;
        ix = match process_mask(mask, bytes, ix, &mut callback) {
            Ok(ix) => std::cmp::max(ix, 16 + block_start),
            Err((end_ix, val)) => return (end_ix, val),
        };
    }

    if bytes.len() > ix {
        // shift off the bytes at start we have already scanned
        let mask = compute_mask(bytes, upperbound as isize) >> ix - upperbound;
        if let Err((end_ix, val)) = process_mask(mask, bytes, ix, &mut callback) {
            return (end_ix, val);
        }
    }

    (bytes.len(), None)
}

/// Scalar fallback.
fn scalar_iterate_special_bytes<F, T>(bytes: &[u8], mut ix: usize, mut callback: F) -> (usize, Option<T>)
    where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    while ix < bytes.len() {
        match callback(ix, bytes[ix]) {
            LoopInstruction::ContinueAndSkip(skip) => {
                ix += skip + 1;
            }
            LoopInstruction::BreakAtWith(ix, val) => {
                return (ix, val);
            }
        }
    }

    (ix, None)
}


#[cfg(all(test, target_arch = "x86_64", feature="simd"))]
mod simd_test {
    use super::{iterate_special_bytes, LoopInstruction};

    fn check_expected_indices(bytes: &[u8], expected: &[usize], skip: usize) {
        let mut indices = vec![];

        iterate_special_bytes::<_, i32>(bytes, 0, |ix, _byte_ty| {
            indices.push(ix);
            LoopInstruction::ContinueAndSkip(skip)
        });

        assert_eq!(&indices[..], expected);
    }

    #[test]
    fn simple_no_match() {
        check_expected_indices("abcdef0123456789".as_bytes(), &[], 0);
    }

    #[test]
    fn simple_match() {
        check_expected_indices("*bcd&f0123456789".as_bytes(), &[0, 4], 0);
    }

    #[test]
    fn single_open_fish() {
        check_expected_indices("<".as_bytes(), &[0], 0);
    }

    #[test]
    fn long_match() {
        check_expected_indices("0123456789abcde~*bcd&f0".as_bytes(), &[15, 16, 20], 0);
    }

    #[test]
    fn border_skip() {
        check_expected_indices("0123456789abcde~~~~d&f0".as_bytes(), &[15, 20], 3);
    }

    #[test]
    fn exhaustive_search() {
        let chars = [b'\n', b'\r', b'*', b'_', b'~', b'|', b'&', b'\\', b'[', b']', b'<', b'!', b'`'];

        for &c in &chars {
            for i in 0u8..=255 {
                if !chars.contains(&i) {
                    // full match
                    let mut buf = [i; 18];
                    buf[3] = c;
                    buf[6] = c;

                    check_expected_indices(&buf[..], &[3, 6], 0);
                }
            }
        }
    }
}