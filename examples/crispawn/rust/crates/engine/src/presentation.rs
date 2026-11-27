// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Text-only view. No filesystem, control, compression or power operations.
use crate::job::JobEvent;

fn clean(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn eta(seconds: Option<u64>) -> String {
    seconds
        .map(|s| format!("{:02}:{:02}:{:02} (概算)", s / 3600, s / 60 % 60, s % 60))
        .unwrap_or_else(|| "計測中/未確定".into())
}
pub fn text(event: &JobEvent) -> String {
    match event {
        JobEvent::State { phase } => format!("[{}]", clean(phase)),
        JobEvent::ProbeTrial { index, total, candidate_id, elapsed_seconds } =>
            format!("[PROBING {index}/{total}] {} {}", clean(candidate_id),
                elapsed_seconds.map(|s| format!("{s:.3}秒")).unwrap_or_else(|| "試験開始".into())),
        JobEvent::PlanSelected { plan, verify_workers, total_bytes, eta_seconds, selection } =>
            format!("[PLAN] 読取={} bytes 圧縮単位={} bytes 並列={} 圧縮={} level={} 確認並列={}\n全体={} bytes ETA={} 選定={}",
                plan.read_bytes, plan.range_bytes, plan.workers, plan.codec.name(), plan.compression_level,
                verify_workers, clean(total_bytes), eta(*eta_seconds), clean(selection)),
        JobEvent::Progress { snapshot: s, percent, eta_seconds, phase } =>
            format!("[{}] {percent}% 読出={} bytes 同期済={} bytes 全体={} bytes 検証読出={} bytes 検証済={} bytes ETA={}",
                clean(phase), s.read_raw_bytes, s.durable_raw_bytes, s.total_bytes, s.verification_read_raw_bytes, s.verified_raw_bytes, eta(*eta_seconds)),
        JobEvent::ReadAhead { read_raw_bytes, queued_encoded_bytes, queue_budget_bytes, .. } =>
            format!("[READ_AHEAD] 読出={} bytes 圧縮済み待機={} / {} bytes", clean(read_raw_bytes), clean(queued_encoded_bytes), clean(queue_budget_bytes)),
        JobEvent::StorageProgress {status,..} => format!("[STORAGE] {} 待ち={} blocks 現在={} bytes 並列={} 読出={:.1} MB/s 書込={:.1} MB/s 圧縮後保存={} 圧縮後確認={} 確認済み元データ換算={}",clean(&status.phase),status.waiting_blocks,status.current_file_bytes,status.write_workers,status.read_mb_per_second,status.write_mb_per_second,status.written_bytes,status.confirmed_bytes,status.confirmed_source_bytes),
        JobEvent::Diagnostic { name, .. } => format!("[DIAG] {}", clean(name)),
        JobEvent::Finished { success, code, report_file, dataset_committed } =>
            format!("[{}] {} 確定済={} レポート={}", if *success { "完了" } else { "停止" },
                clean(code), dataset_committed, clean(&report_file.to_string_lossy())),
        JobEvent::ControlRejected { code, detail } => format!("[CONTROL] {} {}", clean(code), clean(detail)),
    }
}
