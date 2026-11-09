//! Criterion benchmarks for the streaming head parser and a full HTML tree builder.
//!
//! The head parser is measured with whole-document feeds and with pre-sliced 4 KiB and 64 KiB
//! chunks. The full-page head-parser cases feed 4 KiB chunks and 408 KiB chunks; the larger
//! chunk matches wreq-proto's HTTP/1.1 maximum read buffer, so it exposes the tokenizer work
//! that happens after `</head>` on production-sized frames. A whole-document feed would hand
//! body bytes to the tokenizer in the same call that reaches the end of the head, skewing the
//! early-exit comparison.
//!
//! The standard-parser baseline calls `scraper::Html::parse_document`, which builds the entire
//! tree. Dropping that tree is included in the measured time on purpose: the returned [`Html`]
//! is black-boxed and dropped inside every timed iteration, just as a real caller would pay for
//! it.
//!
//! Pages and chunk slices are built before the timed closures, so only parsing is measured.

mod pages;

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use neuradex::metadata::HeadParser;
use scraper::Html;

/// The number of bytes in a kibibyte.
const KIB: usize = 1024;

/// The synthetic head sizes, paired with the labels shown in Criterion reports.
const HEAD_CASES: [(&str, usize); 3] = [
    ("4KiB", 4 * KIB),
    ("64KiB", 64 * KIB),
    ("512KiB", 512 * KIB),
];

/// The body size of the full-page cases.
const BODY_BYTES: usize = 64 * KIB;

/// The chunk size of the small feeds.
const SMALL_CHUNK: usize = 4 * KIB;

/// The chunk size of the large feeds.
const LARGE_CHUNK: usize = 64 * KIB;

/// The chunk size matching wreq-proto's HTTP/1.1 maximum read buffer (408 KiB).
const FRAME_CHUNK: usize = 417_792;

/// Converts a byte count to the `u64` used for Criterion throughput.
///
/// # Panics
///
/// Panics if `len` does not fit in a `u64`, which cannot happen for these pages.
fn throughput_bytes(len: usize) -> u64 {
    u64::try_from(len).expect("benchmark input must fit in u64")
}

/// Builds one page per head case, with the given body size.
fn make_cases(body_bytes: usize) -> Vec<(&'static str, pages::Page)> {
    HEAD_CASES
        .iter()
        .map(|&(label, head_bytes)| (label, pages::make_page(head_bytes, body_bytes)))
        .collect()
}

/// Slices a page into `chunk_size`-byte chunks without copying the document.
fn page_chunks(page: &pages::Page, chunk_size: usize) -> Vec<&[u8]> {
    page.html.as_bytes().chunks(chunk_size).collect()
}

/// Benchmarks the streaming head parser on documents that contain only a head.
fn head_parser_head_only(c: &mut Criterion) {
    let cases = make_cases(0);
    let mut group = c.benchmark_group("head_parser/head_only");

    for (label, page) in &cases {
        group.throughput(Throughput::Bytes(throughput_bytes(page.len())));

        let feeds = [
            ("whole", page_chunks(page, page.len())),
            ("chunked-4KiB", page_chunks(page, SMALL_CHUNK)),
            ("chunked-64KiB", page_chunks(page, LARGE_CHUNK)),
        ];

        for (feed, feed_chunks) in feeds {
            group.bench_with_input(
                BenchmarkId::new(feed, *label),
                feed_chunks.as_slice(),
                |b, chunks| {
                    b.iter(|| {
                        let mut parser = HeadParser::new();

                        for &chunk in chunks {
                            parser.feed(black_box(chunk));
                        }

                        parser.finish();
                        black_box(parser.into_metadata())
                    });
                },
            );
        }
    }

    group.finish();
}

/// Benchmarks the full tree builder on documents that contain only a head.
fn standard_parser_head_only(c: &mut Criterion) {
    let cases = make_cases(0);
    let mut group = c.benchmark_group("standard_parser/head_only");

    for (label, page) in &cases {
        group.throughput(Throughput::Bytes(throughput_bytes(page.len())));

        let html = page.html.as_str();
        group.bench_with_input(BenchmarkId::new("whole", *label), html, |b, html| {
            b.iter(|| black_box(Html::parse_document(black_box(html))));
        });
    }

    group.finish();
}

/// Benchmarks the streaming head parser on documents with a 64 KiB body.
///
/// The `chunked-408KiB` case uses the HTTP/1.1 maximum read buffer that wreq's protocol layer
/// can hand the parser, so it measures the tokenizer work after `</head>` on realistic frame
/// sizes. The 4 KiB case remains as the fine-grained control.
fn head_parser_full_page(c: &mut Criterion) {
    let cases = make_cases(BODY_BYTES);
    let mut group = c.benchmark_group("head_parser/full_page");

    for (label, page) in &cases {
        group.throughput(Throughput::ElementsAndBytes {
            elements: 1,
            bytes: throughput_bytes(page.head_len),
        });

        let feeds = [
            ("chunked-4KiB", page_chunks(page, SMALL_CHUNK)),
            ("chunked-408KiB", page_chunks(page, FRAME_CHUNK)),
        ];

        for (feed, feed_chunks) in feeds {
            group.bench_with_input(
                BenchmarkId::new(feed, *label),
                feed_chunks.as_slice(),
                |b, chunks| {
                    b.iter(|| {
                        let mut parser = HeadParser::new();

                        for &chunk in chunks {
                            parser.feed(black_box(chunk));
                        }

                        parser.finish();
                        black_box(parser.into_metadata())
                    });
                },
            );
        }
    }

    group.finish();
}

/// Benchmarks the full tree builder on documents with a 64 KiB body.
fn standard_parser_full_page(c: &mut Criterion) {
    let cases = make_cases(BODY_BYTES);
    let mut group = c.benchmark_group("standard_parser/full_page");

    for (label, page) in &cases {
        group.throughput(Throughput::ElementsAndBytes {
            elements: 1,
            bytes: throughput_bytes(page.len()),
        });

        let html = page.html.as_str();
        group.bench_with_input(BenchmarkId::new("whole", *label), html, |b, html| {
            b.iter(|| black_box(Html::parse_document(black_box(html))));
        });
    }

    group.finish();
}

/// Runs every parser benchmark group.
fn parser_benchmarks(c: &mut Criterion) {
    head_parser_head_only(c);
    standard_parser_head_only(c);
    head_parser_full_page(c);
    standard_parser_full_page(c);
}

criterion_group! {
    name = benches;
    config = pages::criterion_config();
    targets = parser_benchmarks
}
criterion_main!(benches);
