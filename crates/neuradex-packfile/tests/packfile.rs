//! End-to-end packfile tests through the public API.

use std::fs::{self, File};
use std::io::Write as _;

use neuradex_packfile::{
    ContentHash, FOOTER_HEADER_LEN, FRAME_FLAG_ZSTD, FRAME_HEADER_LEN, Footer, FooterEntry,
    FrameHeader, FrameRef, PackError, PackReader, PackSummary, PackWriter, TRAILER_LEN, Trailer,
    frame_checksum, hash_body,
};

fn build_pack(bodies: &[&[u8]]) -> (Vec<u8>, Vec<ContentHash>, PackSummary) {
    let mut output = Vec::new();
    let mut hashes = Vec::with_capacity(bodies.len());
    let summary = {
        let mut writer = PackWriter::new(&mut output);
        for body in bodies {
            hashes.push(writer.append(body).unwrap());
        }
        writer.finish().unwrap()
    };
    (output, hashes, summary)
}

fn single_body_pack() -> (Vec<u8>, ContentHash, PackSummary) {
    let body = b"the quick brown fox jumps over the lazy dog";
    let (bytes, hashes, summary) = build_pack(&[body]);
    assert_eq!(hashes.len(), 1);
    (bytes, hashes[0], summary)
}

fn assemble_pack(frames: &[u8], footer: &Footer) -> Vec<u8> {
    let footer_bytes = footer.encode().unwrap();
    let footer_offset = u64::try_from(frames.len()).unwrap();
    let footer_len = u64::try_from(footer_bytes.len()).unwrap();

    let mut pack = frames.to_vec();
    pack.extend_from_slice(&footer_bytes);
    let pack_hash = hash_body(&pack);
    let trailer = Trailer::new(
        footer_offset,
        footer_len,
        pack_hash.blake3,
        pack_hash.sha256,
        footer.checksum(),
    );
    pack.extend_from_slice(&trailer.encode());
    pack
}

/// Recomputes only the trailer pack hashes, leaving frames and footer intact.
fn resign_pack(pack: &mut [u8]) {
    let trailer_start = pack.len() - TRAILER_LEN;
    let (content, trailer) = pack.split_at_mut(trailer_start);
    let hash = hash_body(content);
    trailer[32..64].copy_from_slice(&hash.blake3);
    trailer[64..96].copy_from_slice(&hash.sha256);
}

fn footer_offset(summary: &PackSummary) -> usize {
    usize::try_from(summary.footer_offset).unwrap()
}

#[test]
fn round_trip_multiple_bodies() {
    let large: Vec<u8> = (0_u32..1024 * 1024)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect();
    let bodies: Vec<&[u8]> = vec![b"", b"small", b"another body", large.as_slice()];

    let (bytes, hashes, summary) = build_pack(&bodies);
    assert_eq!(summary.entry_count, 4);
    assert_eq!(summary.bytes, u64::try_from(bytes.len()).unwrap());
    assert_eq!(
        summary.footer_offset + summary.footer_len + u64::try_from(TRAILER_LEN).unwrap(),
        summary.bytes
    );

    let reader = PackReader::new(&bytes).unwrap();
    assert_eq!(reader.footer().len(), bodies.len());
    assert!(reader.footer().has_audit());
    assert_eq!(reader.footer().audit().unwrap().len(), bodies.len());
    assert!(
        reader
            .footer()
            .entries()
            .windows(2)
            .all(|pair| pair[0].blake3 < pair[1].blake3)
    );

    let expected_footer = Footer::decode(
        &bytes[footer_offset(&summary)
            ..footer_offset(&summary) + usize::try_from(summary.footer_len).unwrap()],
    )
    .unwrap();
    assert_eq!(&expected_footer, reader.footer());

    let pack_bytes = &bytes[..bytes.len() - TRAILER_LEN];
    let pack_hash = hash_body(pack_bytes);
    assert_eq!(pack_hash.blake3, summary.pack_blake3);
    assert_eq!(pack_hash.sha256, summary.pack_sha256);
    assert_eq!(reader.trailer().footer_offset, summary.footer_offset);
    assert_eq!(reader.trailer().footer_len, summary.footer_len);

    for (body, hash) in bodies.iter().zip(&hashes) {
        assert_eq!(*hash, hash_body(body));
        let entry = reader.lookup(hash).unwrap();
        assert_eq!(entry.blake3, hash.blake3);
        assert_eq!(
            u64::from(entry.plaintext_len),
            u64::try_from(body.len()).unwrap()
        );
        assert_eq!(reader.read(hash).unwrap().as_deref(), Some(*body));

        let frame = reader.frame(entry).unwrap();
        assert_eq!(frame.content_hash(), *hash);
        assert_eq!(frame.header.blake3, hash.blake3);
        assert_eq!(frame.header.sha256, hash.sha256);
        assert_eq!(frame.header.checksum, frame_checksum(frame.compressed));
        assert_eq!(
            u64::try_from(frame.compressed.len()).unwrap(),
            u64::from(frame.header.compressed_len)
        );
        assert_eq!(frame.decode().unwrap(), *body);
    }

    let missing = hash_body(b"not in this pack");
    assert!(reader.lookup(&missing).is_none());
    assert!(reader.read(&missing).unwrap().is_none());

    let mut wrong_sha = hash_body(b"small");
    wrong_sha.sha256[0] ^= 0xff;
    assert!(reader.lookup(&wrong_sha).is_none());
    assert!(reader.read(&wrong_sha).unwrap().is_none());
}

#[test]
fn empty_pack_round_trip() {
    let (bytes, hashes, summary) = build_pack(&[]);
    assert_eq!(hashes.len(), 0);
    assert_eq!(summary.entry_count, 0);
    assert_eq!(summary.footer_offset, 0);
    assert_eq!(
        summary.footer_len,
        u64::try_from(FOOTER_HEADER_LEN).unwrap()
    );
    assert_eq!(summary.bytes, u64::try_from(bytes.len()).unwrap());

    let reader = PackReader::new(&bytes).unwrap();
    assert!(reader.footer().is_empty());
    assert_eq!(reader.footer().len(), 0);
    assert!(reader.read(&hash_body(b"anything")).unwrap().is_none());
    assert!(matches!(
        reader.frame_at(0),
        Err(PackError::EntryIndexOutOfBounds { index: 0, count: 0 })
    ));
}

#[test]
fn writer_works_with_a_file_sink() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pack.ndpk");

    let mut file = File::create(&path).unwrap();
    let summary = {
        let mut writer = PackWriter::new(&mut file);
        writer.append(b"file-backed body").unwrap();
        writer.finish().unwrap()
    };
    file.flush().unwrap();
    drop(file);

    let bytes = fs::read(&path).unwrap();
    assert_eq!(summary.bytes, u64::try_from(bytes.len()).unwrap());
    let reader = PackReader::new(&bytes).unwrap();
    assert_eq!(
        reader
            .read(&hash_body(b"file-backed body"))
            .unwrap()
            .as_deref(),
        Some(&b"file-backed body"[..])
    );
}

#[test]
fn footer_without_audit_table_still_reads() {
    let (bytes, hashes, summary) = build_pack(&[b"one", b"two"]);
    let reader = PackReader::new(&bytes).unwrap();
    let entries = reader.footer().entries().to_vec();
    let footer = Footer::from_entries(entries, None).unwrap();
    let frames = &bytes[..footer_offset(&summary)];
    let pack = assemble_pack(frames, &footer);

    let reader = PackReader::new(&pack).unwrap();
    assert!(!reader.footer().has_audit());
    assert!(reader.footer().audit().is_none());
    assert_eq!(
        reader.read(&hashes[0]).unwrap().as_deref(),
        Some(&b"one"[..])
    );
    assert_eq!(
        reader.read(&hashes[1]).unwrap().as_deref(),
        Some(&b"two"[..])
    );
}

#[test]
fn oversized_duplicate_and_empty_bodies() {
    let mut output = Vec::new();
    let mut writer = PackWriter::new(&mut output);
    let empty = writer.append(b"").unwrap();
    assert_eq!(empty, hash_body(b""));
    assert!(matches!(writer.append(b""), Err(PackError::DuplicateHash)));
    let hash = writer.append(b"after duplicate").unwrap();
    let summary = writer.finish().unwrap();
    assert_eq!(summary.entry_count, 2);

    let reader = PackReader::new(&output).unwrap();
    assert_eq!(reader.read(&empty).unwrap(), Some(Vec::new()));
    assert_eq!(
        reader.read(&hash).unwrap().as_deref(),
        Some(&b"after duplicate"[..])
    );
}

#[test]
fn footer_entry_offset_out_of_bounds_is_rejected() {
    let entry = FooterEntry::new([0xaa; 32], 1_000_000, 10, 5, 0, FRAME_FLAG_ZSTD);
    let footer = Footer::from_entries(vec![entry], None).unwrap();
    let pack = assemble_pack(&[], &footer);

    assert!(matches!(
        PackReader::new(&pack),
        Err(PackError::OutOfBounds { .. })
    ));
}

#[test]
fn trailer_and_footer_layout_corruption_is_rejected() {
    let (bytes, _, summary) = single_body_pack();

    let mut bad_magic = bytes.clone();
    let trailer_start = bad_magic.len() - TRAILER_LEN;
    bad_magic[trailer_start] ^= 0xff;
    assert!(matches!(
        PackReader::new(&bad_magic),
        Err(PackError::InvalidTrailerMagic)
    ));

    let mut bad_version = bytes.clone();
    bad_version[trailer_start + 8] = 9;
    assert!(matches!(
        PackReader::new(&bad_version),
        Err(PackError::UnsupportedVersion {
            found: 9,
            expected: 1
        })
    ));

    let mut bad_reserved = bytes.clone();
    bad_reserved[trailer_start + 12] = 1;
    assert!(matches!(
        PackReader::new(&bad_reserved),
        Err(PackError::ReservedNotZero)
    ));

    let mut bad_footer_len = bytes.clone();
    bad_footer_len[trailer_start + 24..trailer_start + 32]
        .copy_from_slice(&(summary.footer_len + 1).to_le_bytes());
    assert!(matches!(
        PackReader::new(&bad_footer_len),
        Err(PackError::InvalidLayout)
    ));

    let mut bad_footer_checksum_copy = bytes.clone();
    bad_footer_checksum_copy[trailer_start + 96] ^= 0xff;
    assert!(matches!(
        PackReader::new(&bad_footer_checksum_copy),
        Err(PackError::FooterChecksumMismatch)
    ));

    let mut bad_pack_hash = bytes.clone();
    bad_pack_hash[trailer_start + 32] ^= 0xff;
    assert!(matches!(
        PackReader::new(&bad_pack_hash),
        Err(PackError::PackHashMismatch)
    ));

    let footer_start = footer_offset(&summary);
    let mut bad_footer_entry = bytes.clone();
    bad_footer_entry[footer_start + FOOTER_HEADER_LEN] ^= 0xff;
    assert!(matches!(
        PackReader::new(&bad_footer_entry),
        Err(PackError::FooterChecksumMismatch)
    ));

    let mut bad_footer_checksum = bytes;
    bad_footer_checksum[footer_start + 16] ^= 0xff;
    assert!(matches!(
        PackReader::new(&bad_footer_checksum),
        Err(PackError::FooterChecksumMismatch)
    ));
}

#[test]
fn truncated_inputs_are_rejected() {
    let (bytes, _, _) = single_body_pack();

    for cut in 0..bytes.len() {
        assert!(PackReader::new(&bytes[..cut]).is_err(), "cut at {cut}");
    }
    assert!(Trailer::from_pack(&bytes[..TRAILER_LEN - 1]).is_err());
}

#[test]
fn adversarial_mutations_never_panic() {
    let (bytes, _, _) = single_body_pack();
    for index in 0..bytes.len() {
        let mut corrupt = bytes.clone();
        corrupt[index] ^= 0xff;
        if let Ok(reader) = PackReader::new(&corrupt) {
            for entry in reader.footer().entries() {
                let _ = reader.read_entry(entry);
            }
            let _ = reader.read(&hash_body(b"the quick brown fox jumps over the lazy dog"));
        }
    }
}

#[test]
fn frame_header_and_payload_are_verified() {
    let (bytes, _, _) = single_body_pack();

    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 0xff;
    resign_pack(&mut bad_magic);
    let reader = PackReader::new(&bad_magic).unwrap();
    assert!(matches!(
        reader.frame_at(0),
        Err(PackError::InvalidFrameMagic)
    ));

    let mut bad_version = bytes.clone();
    bad_version[4] = 7;
    resign_pack(&mut bad_version);
    let reader = PackReader::new(&bad_version).unwrap();
    assert!(matches!(
        reader.frame_at(0),
        Err(PackError::UnsupportedVersion {
            found: 7,
            expected: 1
        })
    ));

    let mut changed_plaintext_len = bytes.clone();
    changed_plaintext_len[8..16].copy_from_slice(&999_u64.to_le_bytes());
    resign_pack(&mut changed_plaintext_len);
    let reader = PackReader::new(&changed_plaintext_len).unwrap();
    assert!(matches!(reader.frame_at(0), Err(PackError::EntryMismatch)));

    // Mutating the compressed payload without re-signing trips the pack hash.
    let mut changed_payload = bytes;
    changed_payload[FRAME_HEADER_LEN] ^= 0xff;
    assert!(matches!(
        PackReader::new(&changed_payload),
        Err(PackError::PackHashMismatch)
    ));

    // Re-signing lets the pack open; the frame checksum then catches the
    // corruption before decompression.
    resign_pack(&mut changed_payload);
    let reader = PackReader::new(&changed_payload).unwrap();
    assert!(matches!(
        reader.read_entry(reader.footer().entries().first().unwrap()),
        Err(PackError::FrameChecksumMismatch)
    ));
}

#[test]
fn frame_ref_catches_corrupted_zstd_and_hashes() {
    let (bytes, hash, summary) = single_body_pack();
    let reader = PackReader::new(&bytes).unwrap();
    let entry = reader.footer().lookup(&hash).unwrap();
    let start = usize::try_from(entry.frame_offset).unwrap();
    let compressed_start = start + FRAME_HEADER_LEN;
    let compressed_end = compressed_start + usize::try_from(entry.compressed_len).unwrap();
    let header = FrameHeader::decode(&bytes[start..]).unwrap();

    let mut corrupt = bytes[compressed_start..compressed_end].to_vec();
    corrupt[0] ^= 0xff;
    let frame = FrameRef {
        header,
        compressed: &corrupt,
    };
    assert!(matches!(
        frame.decode(),
        Err(PackError::FrameChecksumMismatch)
    ));

    // A checksummed but invalid zstd payload reaches the decoder and fails
    // there, without panicking.
    let invalid = b"not zstd";
    let invalid_frame = FrameRef {
        header: FrameHeader::new(
            3,
            u32::try_from(invalid.len()).unwrap(),
            hash.blake3,
            hash.sha256,
            frame_checksum(invalid),
        ),
        compressed: invalid,
    };
    assert!(matches!(
        invalid_frame.decode(),
        Err(PackError::InvalidZstd(_))
    ));

    assert_eq!(
        summary.footer_offset + summary.footer_len + u64::try_from(TRAILER_LEN).unwrap(),
        summary.bytes
    );
}

#[test]
fn footer_audit_table_must_match_frame_header() {
    let (bytes, _, summary) = single_body_pack();
    let reader = PackReader::new(&bytes).unwrap();
    let entries = reader.footer().entries().to_vec();
    let mut audit = reader.footer().audit().unwrap().to_vec();
    audit[0][0] ^= 0xff;

    let footer = Footer::from_entries(entries, Some(audit)).unwrap();
    let pack = assemble_pack(&bytes[..footer_offset(&summary)], &footer);
    let reader = PackReader::new(&pack).unwrap();
    assert!(matches!(reader.frame_at(0), Err(PackError::EntryMismatch)));
}

#[test]
fn footer_can_be_rebuilt_from_frame_headers() {
    let (bytes, _, summary) = build_pack(&[b"one", b"two", b"three"]);
    let reader = PackReader::new(&bytes).unwrap();

    let mut frames = Vec::new();
    let mut offset = 0_usize;
    while offset < footer_offset(&summary) {
        let header = FrameHeader::decode(&bytes[offset..]).unwrap();
        let compressed_len = usize::try_from(header.compressed_len).unwrap();
        frames.push((
            FooterEntry::new(
                header.blake3,
                u64::try_from(offset).unwrap(),
                header.compressed_len,
                u32::try_from(header.plaintext_len).unwrap(),
                header.checksum,
                header.flags,
            ),
            header.sha256,
        ));
        offset += FRAME_HEADER_LEN + compressed_len;
    }

    assert_eq!(offset, footer_offset(&summary));
    frames.sort_unstable_by_key(|(entry, _)| entry.blake3);
    let (entries, audit): (Vec<_>, Vec<_>) = frames.into_iter().unzip();
    let rebuilt = Footer::from_entries(entries, Some(audit)).unwrap();
    assert_eq!(&rebuilt, reader.footer());
}

#[test]
fn many_entries_are_sorted_and_findable() {
    let bodies: Vec<Vec<u8>> = (0..64)
        .map(|index| format!("body-{index:02}-{}", "x".repeat(index % 17)).into_bytes())
        .collect();
    let slices: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
    let (bytes, hashes, summary) = build_pack(&slices);

    assert_eq!(summary.entry_count, 64);
    let reader = PackReader::new(&bytes).unwrap();
    assert_eq!(reader.footer().len(), 64);
    assert!(
        reader
            .footer()
            .entries()
            .windows(2)
            .all(|pair| pair[0].blake3 < pair[1].blake3)
    );

    for hash in &hashes {
        assert!(reader.footer().lookup(hash).is_some());
        assert!(reader.footer().lookup_blake3(&hash.blake3).is_some());
    }
    for (body, hash) in slices.iter().zip(&hashes) {
        assert_eq!(reader.read(hash).unwrap().as_deref(), Some(*body));
    }
}
