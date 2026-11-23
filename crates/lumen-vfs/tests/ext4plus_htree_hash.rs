//! The vendored `ext4plus`'s htree name hash (#313), signed and unsigned,
//! against e2fsprogs' `ext2fs_dirhash` (the hash `e2fsck` checks an indexed
//! directory's leaves by, and the one Linux's `fs/ext4/hash.c` computes).
//!
//! Each row's four values are what e2fsprogs 1.47.4's `debugfs -R 'dx_hash
//! -h <n> -s <seed> "<name>"'` printed for `n` = 1, 4, 2 and 5: half-MD4
//! signed and unsigned, TEA signed and unsigned (the unsigned versions are
//! Linux's `DX_HASH_*_UNSIGNED`, the algorithm plus 3, which a volume whose
//! `s_flags` has `EXT2_FLAGS_UNSIGNED_HASH` uses). `dx_hash` doesn't take
//! `half_md4_unsigned` by name (it hashes legacy for a name it doesn't know),
//! hence the numbers; `-h 4` gives `é-utf8` the hash `debugfs htree` shows
//! for it in `kernel/src/vfs/testdata/unsigned-hash.ext4`, made by `e2fsck
//! -fyD` on an unsigned-hash volume, with that image's seed (the first one
//! here). ext4plus has no legacy hash (version 0): it refuses such a
//! directory, signed or not, before #313 and after.

use ext4plus::htree_hash;

/// `kernel/src/vfs/testdata/unsigned-hash.ext4`'s `s_hash_seed`.
const IMAGE_SEED: &str = "4c756d65-6e2d-4d37-8000-00000000030c";
/// All zeroes: replaced by the standard default seed.
const ZERO_SEED: &str = "00000000-0000-0000-0000-000000000000";

/// The 255-byte name: every byte from 0x80 to 0xff, twice, less the last.
fn high_bytes() -> Vec<u8> {
    let mut name: Vec<u8> = (0x80..=0xff).chain(0x80..=0xff).collect();
    name.truncate(255);
    name
}

/// A seed as `debugfs` takes it, a UUID, as the four words of `s_hash_seed`
/// (ext4plus's own tests' `seed_from_uuid`).
fn seed_from_uuid(uuid: &str) -> [u32; 4] {
    let hex = uuid.replace('-', "");
    assert_eq!(hex.len(), 32);
    let words: Vec<u32> =
        (0..4).map(|i| u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).unwrap().swap_bytes()).collect();
    words.try_into().unwrap()
}

/// (seed, name, [half-MD4 signed, half-MD4 unsigned, TEA signed, TEA unsigned]).
fn table() -> Vec<(&'static str, Vec<u8>, [u32; 4])> {
    let names: [Vec<u8>; 8] = [
        b"abc".to_vec(),
        // 0x7f, the highest byte the two read alike.
        b"del-\x7f-ascii".to_vec(),
        "é-utf8".as_bytes().to_vec(),
        b"\xff\xfe-odd".to_vec(),
        b"\x80\x81\x82\x83\x84".to_vec(),
        // High bytes in TEA's second 16-byte block only.
        b"sixteen-ascii-ch\xe9\xff\x80\x7f".to_vec(),
        // High bytes in half-MD4's second 32-byte block only.
        [&b"thirty-two-ascii-bytes-for-md4.."[..], "éé💡".as_bytes()].concat(),
        high_bytes(),
    ];
    let image = [
        [0xb37f2762, 0xb37f2762, 0xdd8b0d04, 0xdd8b0d04],
        [0x38d218d6, 0x38d218d6, 0xeb077e00, 0xeb077e00],
        [0x8f4d51ce, 0x22e7cb6c, 0x66abe0da, 0x3e90558c],
        [0x637e7426, 0xa7b63264, 0xb096be96, 0xfd7242f0],
        [0xf057a310, 0x60fe47a2, 0xc070b6ea, 0x47c78490],
        [0x30c0c01c, 0xff654eca, 0xc4f9d8f2, 0xc0152b16],
        [0x2db0d170, 0x979355cc, 0xb4aa5f6c, 0x0e339fc0],
        [0xa984649c, 0xe838c52c, 0x75cbed02, 0x7c53b882],
    ];
    let zero = [
        [0xd196a868, 0xd196a868, 0xb1435ec4, 0xb1435ec4],
        [0xf44945d0, 0xf44945d0, 0x383db96e, 0x383db96e],
        [0x131d3192, 0xa508e194, 0x17b72a10, 0xfdd6a224],
        [0x2378c752, 0x9e4b6d9c, 0xe48b6572, 0x1a1f420a],
        [0x8bab0498, 0xbc48596e, 0x21e3a154, 0x9b648616],
        [0x1ca81cc4, 0xe8c2b01a, 0xd4bd1c34, 0x1d0f6b32],
        [0x1ab98602, 0x1bdd4fec, 0x7cce1cf6, 0x60f12280],
        [0x26324b5c, 0x33329962, 0x6645a6a6, 0xe6833580],
    ];
    let mut rows = Vec::new();
    for (seed, hashes) in [(IMAGE_SEED, image), (ZERO_SEED, zero)] {
        for (name, h) in names.iter().zip(hashes) {
            rows.push((seed, name.clone(), h));
        }
    }
    rows
}

#[test]
fn htree_hashes_match_e2fsprogs_signed_and_unsigned() {
    for (seed, name, [md4_s, md4_u, tea_s, tea_u]) in table() {
        let seed_words = seed_from_uuid(seed);
        let got = [
            htree_hash(1, &name, seed_words, false),
            htree_hash(1, &name, seed_words, true),
            htree_hash(2, &name, seed_words, false),
            htree_hash(2, &name, seed_words, true),
        ];
        let want = [Some(md4_s), Some(md4_u), Some(tea_s), Some(tea_u)];
        assert_eq!(got, want, "seed {seed}, name {:02x?}", name);
        // The two read only bytes of 0x80 or above differently.
        if name.is_ascii() {
            assert_eq!((md4_s, tea_s), (md4_u, tea_u), "{name:02x?}");
        } else {
            assert!(md4_s != md4_u && tea_s != tea_u, "{name:02x?}");
        }
    }
}

#[test]
fn legacy_and_unknown_hash_versions_are_refused_either_way() {
    let seed = seed_from_uuid(IMAGE_SEED);
    for alg in [0, 3, 4, 5, 6, 0xff] {
        for unsigned in [false, true] {
            assert_eq!(htree_hash(alg, "é-utf8".as_bytes(), seed, unsigned), None, "version {alg}");
        }
    }
}

#[test]
fn seed_from_uuid_matches_ext4plus() {
    // ext4plus's own `test_seed_from_uuid`.
    assert_eq!(
        seed_from_uuid("333fa1eb-588c-456e-b81c-d1d343cd0e01"),
        [0xeba13f33, 0x6e458c58, 0xd3d11cb8, 0x010ecd43]
    );
}
