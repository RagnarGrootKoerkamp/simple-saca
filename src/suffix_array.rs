use rayon::prelude::*;
use thread_local::ThreadLocal;

use std::cell::RefCell;
use std::cmp::Ordering;
use std::time::Instant;

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use crate::compact_vec::*;

pub struct SuffixArray<const BYTES: usize> {
    idxs: CompactVec<BYTES>,
    k: usize,
    ctx: usize,
}

const L: usize = 128 - 4;

#[derive(Clone)]
struct Key<const CTX: usize>([__m256i; CTX]);

impl<const BYTES: usize> SuffixArray<BYTES> {
    pub fn new_packed<const CTX: usize>(bytes: &[u8], k: usize, bucket_threads: usize) -> Self {
        let idxs = unsafe { Self::sort_packed::<CTX>(bytes, k, bucket_threads) };

        Self { idxs, k, ctx: CTX }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn sort_packed<const CTX: usize>(
        bytes: &[u8],
        k: usize,
        bucket_threads: usize,
    ) -> CompactVec<BYTES> {
        let k_bits = k * 2;
        let len_no_ctx = bytes.len() - L * CTX;
        let chunk_size = len_no_ctx / bucket_threads;

        let start = Instant::now();
        let packed = RevPacked::new(bytes);
        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("\t2 bit packing run time (s): {elapsed}");

        let start = Instant::now();
        let mut thread_counts = vec![CompactVec::<BYTES>::new(1 << k_bits); bucket_threads];

        rayon::scope(|scope| {
            for (thread_idx, counts) in thread_counts.iter_mut().enumerate() {
                let packed = &packed;
                scope.spawn(move |_| {
                    let start = thread_idx * chunk_size;
                    let end = if thread_idx >= bucket_threads - 1 {
                        len_no_ctx
                    } else {
                        (thread_idx + 1) * chunk_size
                    };

                    for i in start..end {
                        let kmer = packed.load_k(i, k);
                        let count = (*counts.as_ptr().add(kmer as usize)).get_usize();
                        (*counts.as_mut_ptr().add(kmer as usize)).set_usize(count + 1);
                    }
                });
            }
        });

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("\tParallel bucket count run time (s): {elapsed}");

        let mut sum = 0;
        let mut max_bucket = 0;

        let start = Instant::now();

        for i in 0..(1 << k_bits) {
            let mut curr_bucket = 0;

            for thread_idx in 0..bucket_threads {
                let curr = thread_counts[thread_idx][i].get_usize();
                thread_counts[thread_idx][i].set_usize(sum);
                sum += curr;
                curr_bucket += curr;
            }

            max_bucket = max_bucket.max(curr_bucket);
        }

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("\tBucket prefix sum run time (s): {elapsed}");

        let start = Instant::now();
        let mut sorted = CompactVec::<BYTES>::new(len_no_ctx);
        let sorted_ptr = MutPtr(sorted.as_mut_ptr());

        rayon::scope(|scope| {
            for (thread_idx, counts) in thread_counts.iter_mut().enumerate() {
                let packed = &packed;
                scope.spawn(move |_| {
                    let start = thread_idx * chunk_size;
                    let end = if thread_idx >= bucket_threads - 1 {
                        len_no_ctx
                    } else {
                        (thread_idx + 1) * chunk_size
                    };
                    let ptr = sorted_ptr;

                    for i in start..end {
                        let kmer = packed.load_k(i, k);
                        let idx = (*counts.as_ptr().add(kmer as usize)).get_usize();

                        (*ptr.0.add(idx)).set_usize(i);
                        (*counts.as_mut_ptr().add(kmer as usize)).set_usize(idx + 1);
                    }
                });
            }
        });

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("\tParallel move into buckets run time (s): {elapsed}");

        let start = Instant::now();
        let counts = thread_counts.into_iter().last().unwrap();

        // Prevent repeated allocations by using a thread-local vector.
        let keyed_slice_cache: ThreadLocal<RefCell<Vec<_>>> = ThreadLocal::new();

        (0..(1 << k_bits)).into_par_iter().for_each(|i| {
            let start = if i == 0 {
                0
            } else {
                (*counts.as_ptr().add(i - 1)).get_usize()
            };
            let end = (*counts.as_ptr().add(i)).get_usize();
            let ptr = sorted_ptr;
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr.0.add(start), end - start) };

            // For small context, it is more efficient to sort_by_cached_key,
            // while for larger context these keys take up a lot of memory and
            // lookups become more efficient.
            if CTX <= 4 {
                // Inlined sort_by_cached_key. This is slightly more efficient
                // than calling sort_by_cached_key directly, because:
                // 1. Instead of sorting (key, idx_in_slice) pairs, we directly
                //    sort (key, idx_in_string) pairs, removing an indirection
                //    to resolve slice indices back to string indices.
                // 2. It only compares by key, not by index, since we don't need
                //    a stable sort.
                //
                // This makes an extra allocation per thread, but since buckets
                // are typically small compared to the entire suffix array that
                // is OK.

                let mut keyed_slice = keyed_slice_cache.get_or_default().borrow_mut();
                keyed_slice.clear();
                keyed_slice.reserve(slice.len());
                keyed_slice.extend(slice.iter().map(|a_idx| {
                    (
                        simd_key_packed::<CTX>(&packed, a_idx.get_usize()),
                        a_idx.clone(),
                    )
                }));

                keyed_slice.sort_unstable_by(|(kl, _il), (kr, _ir)| key_cmp_packed(kl, kr));
                for i in 0..slice.len() {
                    slice[i] = keyed_slice[i].1.clone();
                }
            } else {
                slice.sort_by(|a_idx, b_idx| unsafe {
                    simd_cmp_packed::<CTX>(&packed, a_idx.get_usize(), b_idx.get_usize())
                });
            }
        });

        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("\tParallel sort buckets run time (s): {elapsed}");
        eprintln!("\tLargest bucket / total: {max_bucket} / {sum}");

        sorted
    }

    pub fn new_bytes<const CTX: usize>(bytes: &[u8]) -> Self {
        let idxs = unsafe { Self::sort_bytes::<CTX>(bytes) };

        Self {
            idxs,
            k: 0,
            ctx: CTX,
        }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn sort_bytes<const CTX: usize>(bytes: &[u8]) -> CompactVec<BYTES> {
        let bytes_no_ctx = &bytes[..bytes.len() - L * CTX];

        let mut sorted = CompactVec::<BYTES>::new(bytes_no_ctx.len());

        for i in 0..bytes_no_ctx.len() {
            sorted[i].set_usize(i);
        }

        sorted.par_sort_by(|a_idx, b_idx| unsafe {
            simd_cmp_bytes::<CTX>(bytes, a_idx.get_usize(), b_idx.get_usize())
        });

        sorted
    }

    pub fn new<const CTX: usize>(seeds: &[u16], k: usize) -> Self {
        assert!(k <= 16);

        let idxs = unsafe { Self::sort::<CTX>(seeds, k) };

        Self { idxs, k, ctx: CTX }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn sort<const CTX: usize>(seeds: &[u16], k: usize) -> CompactVec<BYTES> {
        let seeds_no_ctx = &seeds[..seeds.len() - L * CTX];

        let mut counts = CompactVec::<BYTES>::new(1 << k);

        for &s in seeds_no_ctx {
            let count = counts[s as usize].get_usize();
            counts[s as usize].set_usize(count + 1);
        }

        let mut seed_to_idx = CompactVec::<BYTES>::new((1 << k) + 1);
        let mut idx = 0;
        let mut sum = 0;

        seed_to_idx[idx].set_usize(sum);
        idx += 1;
        for c in counts.iter() {
            sum += c.get_usize();
            seed_to_idx[idx].set_usize(sum);
            idx += 1;
        }

        let mut sorted = CompactVec::<BYTES>::new(seeds_no_ctx.len());

        for (i, &s) in seeds_no_ctx.iter().enumerate() {
            let end = seed_to_idx[(s as usize) + 1].get_usize();
            let count = counts[s as usize].get_usize();

            sorted[end - count].set_usize(i);
            counts[s as usize].set_usize(count - 1);
        }

        drop(counts);

        let sorted_ptr = MutPtr(sorted.as_mut_ptr());

        (0..(1 << k)).into_par_iter().for_each(|i| {
            let start = seed_to_idx[i].get_usize();
            let end = seed_to_idx[i + 1].get_usize();
            let ptr = sorted_ptr;
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr.0.add(start), end - start) };
            slice.sort_by(|a_idx, b_idx| unsafe {
                simd_cmp::<CTX>(seeds, a_idx.get_usize() + 1, b_idx.get_usize() + 1)
            });
        });

        sorted
    }

    pub fn idxs(&self) -> &CompactVec<BYTES> {
        &self.idxs
    }

    pub fn k(&self) -> usize {
        self.k
    }

    pub fn ctx(&self) -> usize {
        self.ctx
    }

    pub fn stats<const CTX: usize>(&self, bytes: &[u8]) {
        let mut cnt = vec![0; 124 * CTX + 1];
        for (x, (i, j)) in self.idxs.iter().zip(self.idxs.iter().skip(1)).enumerate() {
            let i = i.get_usize();
            let j = j.get_usize();
            if x % (1 << 25) == 0 {
                eprint!("Done: {:>4.1}%\r", 100. * x as f32 / self.idxs.len() as f32);

                // let wi: Vec<_> = bytes[i..i + 30].iter().map(|b| LUT[*b as usize]).collect();
                // let wj: Vec<_> = bytes[j..j + 30].iter().map(|b| LUT[*b as usize]).collect();
                // eprintln!("{x:>10} {i:>10} {j:>10} {wi:?} {wj:?}",);
            }
            let mut l = 0;
            while l < 124 * CTX && LUT[bytes[i + l] as usize] == LUT[bytes[j + l] as usize] {
                // eprintln!("{i} {j} {l}: {} == {}", bytes[i + l], bytes[j + l]);
                l += 1;
            }
            // eprintln!("{i} {j} {l}: {} != {}", bytes[i + l], bytes[j + l]);
            cnt[l] += 1;
        }
        eprintln!();
        for (l, c) in cnt.iter().enumerate() {
            eprintln!("{l:>3}: {c:>6}");
        }
    }
}

struct RevPacked {
    data: Vec<u8>,
    len: usize,
}

static LUT: [u8; 128] = {
    let mut l = [0u8; 128];
    l[b'A' as usize] = 0b00;
    l[b'C' as usize] = 0b01;
    l[b'G' as usize] = 0b10;
    l[b'T' as usize] = 0b11;
    l[b'a' as usize] = 0b00;
    l[b'c' as usize] = 0b01;
    l[b'g' as usize] = 0b10;
    l[b't' as usize] = 0b11;
    l
};

impl RevPacked {
    pub fn new(bytes: &[u8]) -> Self {
        let padded_len = bytes.len() + 4;
        let len = (padded_len + 3) / 4;
        let mut data = vec![0u8; len];

        for (i, &b) in bytes.iter().enumerate() {
            let i = padded_len - i - 1;
            unsafe {
                *data.as_mut_ptr().add(i / 4) |= *LUT.as_ptr().add(b as usize) << ((i % 4) * 2);
            }
        }

        Self {
            data,
            len: padded_len,
        }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn load_124(&self, idx: usize) -> __m256i {
        let idx = self.len - idx - 128;
        let i = (idx + 3) / 4;
        let j = (idx + 3) % 4;
        let val = _mm256_loadu_si256(self.data.as_ptr().add(i) as _);

        // shift left by bits
        let left_shift = _mm256_set1_epi64x(((3 - j) * 2) as _);
        let hi = _mm256_sllv_epi64(val, left_shift);
        let right_shift = _mm256_set1_epi64x(((32 - (3 - j)) * 2) as _);
        let lo = _mm256_srlv_epi64(_mm256_permute4x64_epi64(val, 0b10_01_00_11), right_shift);

        let mask = _mm256_set_epi8(
            -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
            -1, -1, -1, -1, -1, -1, -1, -1, -1, 0,
        );

        _mm256_and_si256(_mm256_or_si256(hi, lo), mask)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn load_k(&self, idx: usize, k: usize) -> u32 {
        let idx = self.len - idx - 16;
        let i = (idx + 3) / 4;
        let j = (idx + 3) % 4;
        let val = std::ptr::read_unaligned(self.data.as_ptr().add(i) as *const u32);
        (val << ((3 - j) * 2)) >> ((16 - k) * 2)
    }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn simd_key_packed<const CTX: usize>(packed: &RevPacked, a_idx: usize) -> Key<CTX> {
    let mut a_i = a_idx;

    Key([(); CTX].map(|_| {
        let t = packed.load_124(a_i);
        a_i += L;
        t
    }))
}

#[inline]
unsafe fn cmp_pack(a: __m256i, b: __m256i) -> Ordering {
    let eq = _mm256_cmpeq_epi8(a, b);
    let neq_mask = !(_mm256_movemask_epi8(eq) as u32);

    if neq_mask != 0 {
        let msb_mask = 1u32 << (31 - neq_mask.leading_zeros());
        let gt = _mm256_max_epu8(a, b);
        let gt = _mm256_cmpeq_epi8(gt, a);
        let gt_mask = _mm256_movemask_epi8(gt) as u32;

        if (msb_mask & gt_mask) > 0 {
            return Ordering::Greater;
        } else {
            return Ordering::Less;
        }
    }
    Ordering::Equal
}

#[inline]
fn key_cmp_packed<const CTX: usize>(l: &Key<CTX>, r: &Key<CTX>) -> Ordering {
    unsafe {
        for (&a, &b) in l.0.iter().zip(r.0.iter()) {
            let c = cmp_pack(a, b);
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn simd_cmp_packed<const CTX: usize>(
    packed: &RevPacked,
    a_idx: usize,
    b_idx: usize,
) -> Ordering {
    const L: usize = 128 - 4;
    let mut a_i = a_idx;
    let mut b_i = b_idx;

    for _ in 0..CTX {
        let a = packed.load_124(a_i);
        let b = packed.load_124(b_i);

        let c = cmp_pack(a, b);
        if c != Ordering::Equal {
            return c;
        }

        a_i += L;
        b_i += L;
    }

    a_i.cmp(&b_i)
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn simd_cmp_bytes<const CTX: usize>(bytes: &[u8], a_idx: usize, b_idx: usize) -> Ordering {
    const L: usize = 32;
    let ptr = bytes.as_ptr();
    let mut a_i = a_idx;
    let mut b_i = b_idx;

    for _ in 0..CTX {
        let a = _mm256_loadu_si256(ptr.add(a_i) as _);
        let b = _mm256_loadu_si256(ptr.add(b_i) as _);

        let eq = _mm256_cmpeq_epi8(a, b);
        let neq_mask = !(_mm256_movemask_epi8(eq) as u32);

        if neq_mask != 0 {
            let lsb_mask = neq_mask & neq_mask.wrapping_neg();
            let gt = _mm256_max_epu8(a, b);
            let gt = _mm256_cmpeq_epi8(gt, a);
            let gt_mask = _mm256_movemask_epi8(gt) as u32;

            if (lsb_mask & gt_mask) > 0 {
                return Ordering::Greater;
            } else {
                return Ordering::Less;
            }
        }

        a_i += L;
        b_i += L;
    }

    Ordering::Equal
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn simd_cmp<const CTX: usize>(seeds: &[u16], a_idx: usize, b_idx: usize) -> Ordering {
    const L: usize = 16;
    let ptr = seeds.as_ptr();
    let mut a_i = a_idx;
    let mut b_i = b_idx;

    for _ in 0..CTX {
        let a = _mm256_loadu_si256(ptr.add(a_i) as _);
        let b = _mm256_loadu_si256(ptr.add(b_i) as _);

        let eq = _mm256_cmpeq_epi16(a, b);
        let neq_mask = !(_mm256_movemask_epi8(eq) as u32);

        if neq_mask != 0 {
            let lsb_mask = neq_mask & neq_mask.wrapping_neg();
            let gt = _mm256_max_epu16(a, b);
            let gt = _mm256_cmpeq_epi16(gt, a);
            let gt_mask = _mm256_movemask_epi8(gt) as u32;

            if (lsb_mask & gt_mask) > 0 {
                return Ordering::Greater;
            } else {
                return Ordering::Less;
            }
        }

        a_i += L;
        b_i += L;
    }

    Ordering::Equal
}

#[derive(Copy, Clone)]
struct MutPtr<const BYTES: usize>(*mut Int<BYTES>);
unsafe impl<const BYTES: usize> std::marker::Send for MutPtr<BYTES> {}
unsafe impl<const BYTES: usize> std::marker::Sync for MutPtr<BYTES> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packed() {
        {
            const CTX: usize = 1;
            let mut b = b"ACGTACGT".to_vec();
            b.resize(b.len() + L * CTX, b'A');
            let s = SuffixArray::<5>::new_packed::<CTX>(&b, 1, 1);
            let correct = [4, 0, 5, 1, 6, 2, 7, 3];
            assert_eq!(s.idxs().to_usize_vec(), correct);
        }

        {
            const CTX: usize = 1;
            let mut b = b"ACGTACGT".to_vec();
            b.resize(b.len() + L * CTX, b'A');
            let s = SuffixArray::<5>::new_packed::<CTX>(&b, 2, 1);
            let correct = [4, 0, 5, 1, 6, 2, 7, 3];
            assert_eq!(s.idxs().to_usize_vec(), correct);
        }

        {
            const CTX: usize = 2;
            let mut b = b"ACGTACGT".to_vec();
            b.resize(b.len() + L * CTX, b'A');
            let s = SuffixArray::<5>::new_packed::<CTX>(&b, 2, 1);
            let correct = [4, 0, 5, 1, 6, 2, 7, 3];
            assert_eq!(s.idxs().to_usize_vec(), correct);
        }

        {
            const CTX: usize = 1;
            let mut b = b"TTTT".to_vec();
            b.resize(b.len() + L * CTX, b'A');
            let s = SuffixArray::<5>::new_packed::<CTX>(&b, 2, 1);
            let correct = [3, 2, 1, 0];
            assert_eq!(s.idxs().to_usize_vec(), correct);
        }

        {
            const CTX: usize = 1;
            let mut b = b"AAAAAAAAACAAAAAAAAAT".to_vec();
            b.resize(b.len() + L * CTX, b'A');
            let s = SuffixArray::<5>::new_packed::<CTX>(&b, 2, 1);
            s.stats::<CTX>(&b);
            let correct = [];
            assert_eq!(s.idxs().to_usize_vec(), correct);
        }
    }
}
