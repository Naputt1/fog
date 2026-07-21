use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fog::terminal::Terminal;

fn make_term(content_lines: usize) -> Terminal {
    let content = (0..content_lines)
        .map(|i| format!("line {i} with some content here and more text"))
        .collect::<Vec<_>>()
        .join("\r\n");
    Terminal::spawn_error("test".to_string(), content, 2000)
}

fn make_ansi_term(content_lines: usize) -> Terminal {
    let mut content = String::new();
    for i in 0..content_lines {
        content.push_str(&format!("\x1b[31mRed \x1b[1mlight {i}\x1b[0m\r\n"));
    }
    Terminal::spawn_error("test".to_string(), content, 2000)
}

/// Warm benchmarks: reuse the same Terminal across iterations (measures cache-hit path after first call).

fn bench_get_screen_empty(c: &mut Criterion) {
    let term = Terminal::spawn_error("test".to_string(), String::new(), 2000);
    c.bench_function("get_screen/empty", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(0));
            black_box(r)
        })
    });
}

fn bench_get_screen_small(c: &mut Criterion) {
    let term = make_term(10);
    c.bench_function("get_screen/10_lines_plain", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(0));
            black_box(r)
        })
    });
}

fn bench_get_screen_medium(c: &mut Criterion) {
    let term = make_term(200);
    c.bench_function("get_screen/200_lines_plain", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(0));
            black_box(r)
        })
    });
}

fn bench_get_screen_large(c: &mut Criterion) {
    let term = make_term(1000);
    c.bench_function("get_screen/1000_lines_plain", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(0));
            black_box(r)
        })
    });
}

fn bench_get_screen_ansi_medium(c: &mut Criterion) {
    let term = make_ansi_term(200);
    c.bench_function("get_screen/200_lines_ansi", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(0));
            black_box(r)
        })
    });
}

fn bench_get_screen_scrolled(c: &mut Criterion) {
    let term = make_term(500);
    c.bench_function("get_screen/500_lines_scrolled_100", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(100));
            black_box(r)
        })
    });
}

fn bench_get_screen_cache_hit(c: &mut Criterion) {
    let term = make_term(200);
    let _ = term.get_screen(24, 0);
    c.bench_function("get_screen/cache_hit", |b| {
        b.iter(|| {
            let r = term.get_screen(black_box(24), black_box(0));
            black_box(r)
        })
    });
}

/// Cold benchmarks: fresh Terminal per iteration (always a cache miss, measures the true render cost).

fn bench_get_screen_cold_small(c: &mut Criterion) {
    c.bench_function("get_screen/cold_10_lines", |b| {
        b.iter_batched(
            || make_term(10),
            |term| {
                let r = term.get_screen(black_box(24), black_box(0));
                black_box(r)
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn bench_get_screen_cold_medium(c: &mut Criterion) {
    c.bench_function("get_screen/cold_200_lines", |b| {
        b.iter_batched(
            || make_term(200),
            |term| {
                let r = term.get_screen(black_box(24), black_box(0));
                black_box(r)
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn bench_get_screen_cold_large(c: &mut Criterion) {
    c.bench_function("get_screen/cold_1000_lines", |b| {
        b.iter_batched(
            || make_term(1000),
            |term| {
                let r = term.get_screen(black_box(24), black_box(0));
                black_box(r)
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn bench_get_screen_cold_ansi(c: &mut Criterion) {
    c.bench_function("get_screen/cold_200_ansi", |b| {
        b.iter_batched(
            || make_ansi_term(200),
            |term| {
                let r = term.get_screen(black_box(24), black_box(0));
                black_box(r)
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn bench_get_screen_cold_scrolled(c: &mut Criterion) {
    c.bench_function("get_screen/cold_500_scrolled_100", |b| {
        b.iter_batched(
            || make_term(500),
            |term| {
                let r = term.get_screen(black_box(24), black_box(100));
                black_box(r)
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_get_screen_empty,
    bench_get_screen_small,
    bench_get_screen_medium,
    bench_get_screen_large,
    bench_get_screen_ansi_medium,
    bench_get_screen_scrolled,
    bench_get_screen_cache_hit,
    bench_get_screen_cold_small,
    bench_get_screen_cold_medium,
    bench_get_screen_cold_large,
    bench_get_screen_cold_ansi,
    bench_get_screen_cold_scrolled,
);
criterion_main!(benches);
