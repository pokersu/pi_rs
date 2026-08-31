//! Rust 翻译自 packages/telemetry/src/testing/conformance.ts
//!
//! 为 callback telemetry adapter 契约创建与运行器无关的 conformance case。
//!
//! 注：TS 原版另有 3 个依赖 JS `Proxy`（构造“不可读对象”）的 passivity case，
//! 用于验证 adapter 对畸形 payload 的被动容错。Rust 的静态类型系统下不存在
//! “属性读取时抛错”的对象，故这些 case 无法等价表达，在此省略。

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::FutureExt;

use crate::{
    AttributeValue, RecordedTelemetryEvent, RecordedTelemetrySpan, SpanAttributes, SpanError,
    SpanOptions, SpanStatus, TelemetryContext, TelemetrySpan,
};

use super::types::{
    TelemetryAdapterConformanceCase, TelemetryAdapterFixture, TelemetryAdapterFixtureFactory,
};

type ConformanceTest = Arc<
    dyn Fn(Arc<TelemetryAdapterFixture>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// 对应 `createCase`
fn create_case(
    factory: TelemetryAdapterFixtureFactory,
    group: &'static str,
    name: &'static str,
    test: ConformanceTest,
) -> TelemetryAdapterConformanceCase {
    TelemetryAdapterConformanceCase {
        group,
        name,
        run: Box::new(move || {
            let fixture = Arc::new(factory());
            let test = test.clone();
            Box::pin(async move { test(fixture).await })
        }),
    }
}

/// 对应 `findSpan`
fn find_span<'a>(spans: &'a [RecordedTelemetrySpan], name: &str) -> &'a RecordedTelemetrySpan {
    spans
        .iter()
        .find(|candidate| candidate.name == name)
        .unwrap_or_else(|| panic!("Expected recorded span {name}"))
}

// --- 测试辅助构造 ---

fn attr_str(v: &str) -> AttributeValue {
    AttributeValue::String(v.to_string())
}

fn attr_bool(v: bool) -> AttributeValue {
    AttributeValue::Boolean(v)
}

fn attr_num(v: f64) -> AttributeValue {
    AttributeValue::Number(v)
}

fn attrs(pairs: &[(&str, Option<AttributeValue>)]) -> SpanAttributes {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

fn opts(name: &str, attributes: Option<SpanAttributes>) -> SpanOptions {
    SpanOptions {
        name: name.to_string(),
        attributes,
    }
}

/// 对应 `createTelemetryAdapterConformance`
pub fn create_telemetry_adapter_conformance(
    factory: TelemetryAdapterFixtureFactory,
) -> Vec<TelemetryAdapterConformanceCase> {
    vec![
        // 1. callback lifecycle / admits once synchronously and preserves the result
        create_case(
            factory.clone(),
            "callback lifecycle",
            "admits once synchronously and preserves the result",
            Arc::new(|fixture| {
                Box::pin(async move {
                    let admitted = std::sync::atomic::AtomicBool::new(false);
                    let calls = std::sync::atomic::AtomicU32::new(0);
                    let expected = 42u32;
                    let result = fixture
                        .context
                        .start_span(opts("success", None), {
                            let admitted = &admitted;
                            let calls = &calls;
                            move |_span| async move {
                                admitted.store(true, std::sync::atomic::Ordering::Relaxed);
                                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                expected
                            }
                        })
                        .await;

                    assert!(admitted.load(std::sync::atomic::Ordering::Relaxed));
                    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
                    assert_eq!(result, expected);
                    let spans = fixture.get_spans().await;
                    assert_eq!(find_span(&spans, "success").status, SpanStatus::Ok);
                    assert!(find_span(&spans, "success").settled);
                })
            }),
        ),
        // 2. callback lifecycle / preserves synchronous and asynchronous rejection values
        create_case(
            factory.clone(),
            "callback lifecycle",
            "preserves synchronous and asynchronous rejection values",
            Arc::new(|fixture| {
                Box::pin(async move {
                    let sync_result = AssertUnwindSafe(fixture.context.start_span(
                        opts("sync-error", None),
                        |_span| async move { panic!("sync") },
                    ))
                    .catch_unwind()
                    .await;
                    assert!(sync_result.is_err());

                    let async_result = AssertUnwindSafe(fixture.context.start_span(
                        opts("async-error", None),
                        |_span| async move { panic!("async") },
                    ))
                    .catch_unwind()
                    .await;
                    assert!(async_result.is_err());

                    let spans = fixture.get_spans().await;
                    for name in ["sync-error", "async-error"] {
                        assert!(matches!(
                            find_span(&spans, name).status,
                            SpanStatus::Error { .. }
                        ));
                    }
                })
            }),
        ),
        // 3. status / uses last explicit status without automatic overwrite
        create_case(
            factory.clone(),
            "status",
            "uses last explicit status without automatic overwrite",
            Arc::new(|fixture| {
                Box::pin(async move {
                    fixture
                        .context
                        .start_span(opts("last-status", None), |span| async move {
                            span.set_status(SpanStatus::Error {
                                error: Some(SpanError {
                                    name: "Expected".into(),
                                    message: "first".into(),
                                }),
                            });
                            span.set_status(SpanStatus::Ok);
                        })
                        .await;

                    let thrown = AssertUnwindSafe(fixture.context.start_span(
                        opts("explicit-before-throw", None),
                        |span| async move {
                            span.set_status(SpanStatus::Ok);
                            panic!("after explicit status");
                        },
                    ))
                    .catch_unwind()
                    .await;
                    assert!(thrown.is_err());

                    let rejected = AssertUnwindSafe(fixture.context.start_span(
                        opts("explicit-before-rejection", None),
                        |span| async move {
                            span.set_status(SpanStatus::Error {
                                error: Some(SpanError {
                                    name: "Expected".into(),
                                    message: "async failure".into(),
                                }),
                            });
                            panic!("rejected");
                        },
                    ))
                    .catch_unwind()
                    .await;
                    assert!(rejected.is_err());

                    fixture
                        .context
                        .start_span(opts("expected-failure", None), |span| async move {
                            span.set_status(SpanStatus::Error {
                                error: Some(SpanError {
                                    name: "Expected".into(),
                                    message: "returned failure".into(),
                                }),
                            });
                            false
                        })
                        .await;

                    let spans = fixture.get_spans().await;
                    assert_eq!(find_span(&spans, "last-status").status, SpanStatus::Ok);
                    assert_eq!(
                        find_span(&spans, "explicit-before-throw").status,
                        SpanStatus::Ok
                    );
                    assert_eq!(
                        find_span(&spans, "explicit-before-rejection").status,
                        SpanStatus::Error {
                            error: Some(SpanError {
                                name: "Expected".into(),
                                message: "async failure".into()
                            })
                        }
                    );
                    assert_eq!(
                        find_span(&spans, "expected-failure").status,
                        SpanStatus::Error {
                            error: Some(SpanError {
                                name: "Expected".into(),
                                message: "returned failure".into()
                            })
                        }
                    );
                })
            }),
        ),
        // 4. recording / merges attributes and records ordered events
        create_case(
            factory.clone(),
            "recording",
            "merges attributes and records ordered events",
            Arc::new(|fixture| {
                Box::pin(async move {
                    fixture
                        .context
                        .start_span(
                            opts(
                                "recording",
                                Some(attrs(&[
                                    ("start", Some(attr_str("value"))),
                                    ("overwrite", Some(attr_str("start"))),
                                    ("ignored", None),
                                ])),
                            ),
                            |span| async move {
                                span.set_attributes(attrs(&[
                                    ("count", Some(attr_num(1.0))),
                                    ("overwrite", Some(attr_str("middle"))),
                                ]));
                                span.set_attributes(attrs(&[
                                    ("count", None),
                                    ("overwrite", Some(attr_str("end"))),
                                ]));
                                span.add_event(
                                    "first",
                                    Some(attrs(&[
                                        ("index", Some(attr_num(1.0))),
                                        ("ignored", None),
                                    ])),
                                );
                                span.add_event(
                                    "second",
                                    Some(attrs(&[("index", Some(attr_num(2.0)))])),
                                );
                            },
                        )
                        .await;

                    let spans = fixture.get_spans().await;
                    let span = find_span(&spans, "recording");
                    assert_eq!(
                        span.attributes,
                        attrs(&[
                            ("start", Some(attr_str("value"))),
                            ("overwrite", Some(attr_str("end"))),
                            ("count", Some(attr_num(1.0))),
                        ])
                    );
                    assert_eq!(
                        span.events,
                        vec![
                            RecordedTelemetryEvent {
                                name: "first".into(),
                                attributes: attrs(&[("index", Some(attr_num(1.0)))])
                            },
                            RecordedTelemetryEvent {
                                name: "second".into(),
                                attributes: attrs(&[("index", Some(attr_num(2.0)))])
                            },
                        ]
                    );
                })
            }),
        ),
        // 6. recording / makes calls after settlement inert
        create_case(
            factory.clone(),
            "recording",
            "makes calls after settlement inert",
            Arc::new(|fixture| {
                Box::pin(async move {
                    let settled_span: Arc<Mutex<Option<Arc<dyn TelemetrySpan>>>> =
                        Arc::new(Mutex::new(None));
                    let slot = settled_span.clone();
                    fixture
                        .context
                        .start_span(
                            opts(
                                "settled",
                                Some(attrs(&[("value", Some(attr_str("initial")))])),
                            ),
                            |span| async move {
                                *slot.lock().unwrap() = Some(span);
                            },
                        )
                        .await;
                    let captured = settled_span.lock().unwrap().clone().unwrap();

                    captured.set_attributes(attrs(&[("value", Some(attr_str("late")))]));
                    captured.add_event("late", Some(attrs(&[("value", Some(attr_bool(true)))])));
                    captured.set_status(SpanStatus::Error { error: None });
                    let child_result = captured
                        .start_child_span(
                            opts("late-child", None),
                            Box::new(|_span| {
                                Box::pin(async move { Box::new(7i32) as Box<dyn Any + Send> })
                            }),
                        )
                        .await;
                    let child_value = child_result.downcast::<i32>().unwrap();
                    assert_eq!(*child_value, 7);

                    let spans = fixture.get_spans().await;
                    assert_eq!(spans.len(), 1);
                    assert_eq!(
                        spans[0].attributes,
                        attrs(&[("value", Some(attr_str("initial")))])
                    );
                    assert!(spans[0].events.is_empty());
                    assert_eq!(spans[0].status, SpanStatus::Ok);
                })
            }),
        ),
        // 7. parentage / records nested and concurrent child relationships
        create_case(
            factory.clone(),
            "parentage",
            "records nested and concurrent child relationships",
            Arc::new(|fixture| {
                Box::pin(async move {
                    fixture
                        .context
                        .start_span(opts("parent", None), |parent| async move {
                            let (tx, rx) = futures::channel::oneshot::channel::<()>();
                            let rx = Arc::new(Mutex::new(Some(rx)));
                            let tx = Arc::new(Mutex::new(Some(tx)));

                            let first_rx = rx.clone();
                            let first = parent.start_child_span(
                                opts("first-child", None),
                                Box::new(move |_span| {
                                    Box::pin(async move {
                                        let rx = first_rx.lock().unwrap().take().unwrap();
                                        let _ = rx.await;
                                        Box::new(()) as Box<dyn Any + Send>
                                    })
                                }),
                            );

                            let second = parent.start_child_span(
                                opts("second-child", None),
                                Box::new(|_span| {
                                    Box::pin(async move {
                                        Box::new("done".to_string()) as Box<dyn Any + Send>
                                    })
                                }),
                            );

                            let second_value = second.await.downcast::<String>().unwrap();
                            assert_eq!(*second_value, "done");

                            tx.lock().unwrap().take().unwrap().send(()).ok();
                            let _ = first.await;
                        })
                        .await;

                    let spans = fixture.get_spans().await;
                    let parent = find_span(&spans, "parent");
                    let first = find_span(&spans, "first-child");
                    let second = find_span(&spans, "second-child");
                    assert_eq!(parent.parent_id, None);
                    assert_eq!(first.parent_id, Some(parent.id));
                    assert_eq!(second.parent_id, Some(parent.id));
                    assert!(
                        second.end_sequence.is_some()
                            && first.end_sequence.is_some()
                            && parent.end_sequence.is_some()
                    );
                    assert!(second.end_sequence.unwrap() < first.end_sequence.unwrap());
                    assert!(first.end_sequence.unwrap() < parent.end_sequence.unwrap());
                })
            }),
        ),
    ]
}
