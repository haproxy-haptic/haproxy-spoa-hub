use criterion::{Criterion, black_box, criterion_group, criterion_main};
use haproxy_spoa_hub::dispatch::{spoe_value_to_typed_data, typed_data_to_spoe_value};
use spop::types::TypedData;

fn bench_typed_data_to_spoe_value(c: &mut Criterion) {
    let mut group = c.benchmark_group("typed_data_to_spoe_value");

    group.bench_function("null", |b| {
        b.iter(|| typed_data_to_spoe_value(black_box(&TypedData::Null)));
    });

    group.bench_function("uint32", |b| {
        let td = TypedData::UInt32(42);
        b.iter(|| typed_data_to_spoe_value(black_box(&td)));
    });

    group.bench_function("string_short", |b| {
        let td = TypedData::String("hello".to_string());
        b.iter(|| typed_data_to_spoe_value(black_box(&td)));
    });

    group.bench_function("string_long", |b| {
        let td = TypedData::String("x".repeat(1024));
        b.iter(|| typed_data_to_spoe_value(black_box(&td)));
    });

    group.bench_function("ipv4", |b| {
        let td = TypedData::IPv4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        b.iter(|| typed_data_to_spoe_value(black_box(&td)));
    });

    group.bench_function("binary_256b", |b| {
        let td = TypedData::Binary(vec![0xAB; 256]);
        b.iter(|| typed_data_to_spoe_value(black_box(&td)));
    });

    group.finish();
}

fn bench_spoe_value_to_typed_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("spoe_value_to_typed_data");

    let val = typed_data_to_spoe_value(&TypedData::String("hello".to_string()));
    group.bench_function("string_short", |b| {
        b.iter(|| spoe_value_to_typed_data(black_box(&val)));
    });

    let val = typed_data_to_spoe_value(&TypedData::UInt32(42));
    group.bench_function("uint32", |b| {
        b.iter(|| spoe_value_to_typed_data(black_box(&val)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_typed_data_to_spoe_value,
    bench_spoe_value_to_typed_data
);
criterion_main!(benches);
