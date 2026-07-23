mod common;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use spop::types::TypedData;

/// Connection-level pipelining regression test.
///
/// The hub advertises the SPOP `pipelining` capability, so HAProxy streams
/// many NOTIFY frames on one connection without waiting for each ACK. Those
/// frames must be processed CONCURRENTLY. A serial per-connection loop (read →
/// await dispatch → ACK → read next) processes them one at a time, so N frames
/// with a D-ms plugin take ~N*D; concurrent processing takes ~D.
///
/// With N=6 frames of 50 ms each, serial ≈ 300 ms and concurrent ≈ 50 ms. The
/// plugin's admission ceiling is pinned above N so this measures the
/// connection loop, not per-plugin serialization.
#[tokio::test]
async fn test_pipelined_frames_process_concurrently() {
    let socket_path = common::unique_sock("pipeline");

    let mut cfg = common::delay_config("delay", "check-pipeline", 2000);
    cfg.max_concurrency = Some(32); // well above N, so the plugin never gates
    let registry = common::build_registry(&[cfg]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    let args = HashMap::from([("delay_ms".to_string(), TypedData::UInt32(50))]);
    const N: u64 = 6;

    let start = Instant::now();

    // Pipeline: write all N NOTIFY frames (distinct stream/frame IDs) before
    // reading any ACK.
    for i in 1..=N {
        client
            .write_notify(i, i, "check-pipeline", &args)
            .await
            .unwrap();
    }
    client.flush().await.unwrap();

    // Collect N ACKs — under concurrent processing they may arrive in any
    // order, so match by the echoed frame ID.
    let mut frame_ids = Vec::new();
    for _ in 0..N {
        let ack = client.read_ack().await.unwrap();
        assert!(
            ack.find_var("delay.done").is_some(),
            "delay.done missing on frame {}, got: {:?}",
            ack.frame_id,
            ack.actions
        );
        frame_ids.push(ack.frame_id);
    }
    let elapsed = start.elapsed();

    frame_ids.sort_unstable();
    assert_eq!(
        frame_ids,
        (1..=N).collect::<Vec<_>>(),
        "every pipelined frame must be acked exactly once"
    );

    // Serial would be N*50 = 300 ms; concurrent is ~50 ms. Assert well below
    // the serial time, with generous margin for CI scheduling jitter.
    assert!(
        elapsed < Duration::from_millis(200),
        "expected concurrent frame processing (<200ms for {N}x50ms), took {elapsed:?}; \
         a serial per-connection loop would take ~{}ms",
        N * 50,
    );

    common::cleanup(client, server, Some(&socket_path)).await;
}

/// HAPROXY-DISCONNECT while frames are still in flight must deliver every
/// in-flight ACK before the connection closes, then wind down cleanly.
///
/// Under the concurrent loop a disconnect arrives while spawned per-frame
/// tasks are still running. The read loop stops, the writer drains every
/// in-flight ACK, and only then is AGENT-DISCONNECT sent (SPOP forbids frames
/// after it). This test reads all the ACKs back so it actually catches a
/// dropped or lost reply — not just a hang: the four frames' ACKs must all
/// arrive (in any order), and the server task must return without panicking.
#[tokio::test]
async fn test_disconnect_drains_inflight_frames() {
    let socket_path = common::unique_sock("disconnect-pipeline");

    let mut cfg = common::delay_config("delay", "check-disc", 2000);
    cfg.max_concurrency = Some(32);
    let registry = common::build_registry(&[cfg]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    // Pipeline several slow frames, then disconnect before any can finish.
    let args = HashMap::from([("delay_ms".to_string(), TypedData::UInt32(80))]);
    const N: u64 = 4;
    for i in 1..=N {
        client
            .write_notify(i, i, "check-disc", &args)
            .await
            .unwrap();
    }
    client.flush().await.unwrap();
    client.disconnect().await.unwrap();

    // Every in-flight frame's ACK must still be delivered — read all N back and
    // confirm each frame id returned (draining before close drops nothing). The
    // ACKs precede AGENT-DISCONNECT, so all N reads see ACK frames.
    let mut frame_ids = Vec::new();
    for _ in 0..N {
        let ack = client.read_ack().await.unwrap();
        assert!(
            ack.find_var("delay.done").is_some(),
            "delay.done missing on frame {}, got: {:?}",
            ack.frame_id,
            ack.actions
        );
        frame_ids.push(ack.frame_id);
    }
    frame_ids.sort_unstable();
    assert_eq!(
        frame_ids,
        (1..=N).collect::<Vec<_>>(),
        "every in-flight ACK must be delivered on disconnect, none dropped"
    );

    // The connection task then winds down cleanly (AGENT-DISCONNECT sent last).
    let shutdown = tokio::time::timeout(Duration::from_secs(3), server).await;
    assert!(
        shutdown.is_ok(),
        "server did not shut down within 3s after disconnect with in-flight frames (hang)"
    );
    assert!(
        shutdown.unwrap().is_ok(),
        "server task panicked while draining in-flight frames on disconnect"
    );

    let _ = std::fs::remove_file(&socket_path);
}

/// The shared in-flight gate bounds total concurrency and backpressures.
///
/// With a gate of 2 and the plugin's own ceiling well above that, four
/// pipelined 50 ms frames must process at most two at a time — so they complete
/// in ~2 waves (~100 ms), not ~50 ms (all four at once, no gate) and not
/// ~200 ms (fully serial). This is what makes the read loop stop reading and
/// backpressure HAProxy under load instead of overloading the hub.
#[tokio::test]
async fn test_inflight_gate_backpressures() {
    let socket_path = common::unique_sock("gate");

    let mut cfg = common::delay_config("delay", "check-gate", 2000);
    cfg.max_concurrency = Some(32); // the plugin is not the limit; the gate is
    let registry = common::build_registry(&[cfg]);
    let (mut client, server) =
        common::start_server_and_connect_gated(&socket_path, registry, 2).await;

    let args = HashMap::from([("delay_ms".to_string(), TypedData::UInt32(50))]);
    const N: u64 = 4;

    let start = Instant::now();
    for i in 1..=N {
        client
            .write_notify(i, i, "check-gate", &args)
            .await
            .unwrap();
    }
    client.flush().await.unwrap();
    let mut frame_ids = Vec::new();
    for _ in 0..N {
        frame_ids.push(client.read_ack().await.unwrap().frame_id);
    }
    let elapsed = start.elapsed();

    frame_ids.sort_unstable();
    assert_eq!(
        frame_ids,
        (1..=N).collect::<Vec<_>>(),
        "every frame must ack"
    );

    // >= ~2 waves: a gate of 2 must have prevented all four running at once
    // (which would be ~50 ms). This is the backpressure taking effect.
    assert!(
        elapsed >= Duration::from_millis(80),
        "gate=2 must limit concurrency to two (~2 waves), took {elapsed:?} (no backpressure?)"
    );
    // ...but still ran two concurrently, so well under the ~200 ms serial time.
    assert!(
        elapsed < Duration::from_millis(190),
        "gate=2 should still run two concurrently (< serial), took {elapsed:?}"
    );

    common::cleanup(client, server, Some(&socket_path)).await;
}

/// A HAPROXY-DISCONNECT arriving while the read loop is backpressured on a full
/// gate must be acted on promptly, not stalled behind the permit wait.
///
/// With a gate of 1, the first frame takes the permit and the rest back up. The
/// loop, parked waiting for a permit, still watches the reader — so when the
/// disconnect arrives it winds down instead of FIFO-processing every backed-up
/// frame first. The already-running frame's ACK is still delivered; the
/// backpressured frames are dropped. (Contrast test_disconnect_drains_inflight_
/// frames, where a large gate means nothing is backpressured and all ACKs land.)
#[tokio::test]
async fn test_disconnect_while_backpressured_is_prompt() {
    let socket_path = common::unique_sock("bp-disconnect");

    let mut cfg = common::delay_config("delay", "check-bpd", 2000);
    cfg.max_concurrency = Some(32); // the plugin is not the limit; the gate is
    let registry = common::build_registry(&[cfg]);
    let (mut client, server) =
        common::start_server_and_connect_gated(&socket_path, registry, 1).await;

    let args = HashMap::from([("delay_ms".to_string(), TypedData::UInt32(80))]);
    const N: u64 = 5;
    for i in 1..=N {
        client.write_notify(i, i, "check-bpd", &args).await.unwrap();
    }
    client.flush().await.unwrap();
    client.disconnect().await.unwrap();

    // Read ACKs until the server sends AGENT-DISCONNECT / closes.
    let mut acks = 0usize;
    while client.read_ack().await.is_ok() {
        acks += 1;
    }

    // The in-flight frame's ACK is delivered...
    assert!(
        acks >= 1,
        "in-flight frame's ACK must be delivered, got {acks}"
    );
    // ...but the disconnect was caught mid-backpressure, so the remaining
    // frames were dropped rather than FIFO-processed (which would yield N acks).
    assert!(
        acks < N as usize,
        "backpressured frames must be dropped on disconnect, got {acks}/{N} acks (stalled?)"
    );

    let shutdown = tokio::time::timeout(Duration::from_secs(2), server).await;
    assert!(
        shutdown.is_ok(),
        "server hung after disconnect while backpressured"
    );
    assert!(shutdown.unwrap().is_ok(), "server task panicked");

    let _ = std::fs::remove_file(&socket_path);
}
