// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Product text shared by the GUI and human-facing terminal output.
use crate::{presentation::Language, Error, Result};

#[derive(Debug, Clone, Copy)]
pub enum Text {
    Copy,
    Restore,
    Selected,
    Switch,
    Locked,
    ModeLocked,
    SelectSources,
    Copying,
    Restoring,
    Finish,
    CopyTitle,
    ChangeSources,
    Preparing,
    Ready,
    Countdown,
    ManualStart,
    NoDisks,
    NoBackups,
    RestoreSource,
    RestoreMapping,
    RestoreReview,
    OverwriteWarning,
    Start,
    Next,
    Back,
    Pause,
    Resume,
    Cancel,
    Details,
    LogsTitle,
    LogsBack,
    LogsControls,
    AbortTitle,
    Continue,
    Abort,
    AbortCopyWarning,
    AbortRestoreWarning,
    Overall,
    Eta,
    Estimating,
    Read,
    Durable,
    Written,
    Confirmed,
    Total,
    DoneCount,
    ReadBlock,
    Compression,
    Workers,
    VerifyWorkers,
    Frame,
    Hash,
    Destination,
    Filesystem,
    Free,
    Done,
    Working,
    Waiting,
    Unavailable,
    SourceKeys,
    TargetKeys,
    PageKeys,
    AllFinished,
    Shutdown,
    CopyTail,
    RestoreTail,
    PreparationFailed,
    SelectedCount,
    CurrentDisk,
    UsedBackup,
    SourceDisk,
    TargetDisk,
    RestoreStart,
    WaitingForConfirmation,
    Failure,
    Cancelled,
    Poweroff,
    Logging,
    UsbDisk,
    ErrorLowSpace,
    ErrorFull,
    ErrorIo,
    ErrorBootIso,
    ErrorDevice,
    ErrorReadOnly,
    ErrorSource,
    ErrorConfig,
    ErrorVerification,
    ErrorTimeout,
    ErrorMemory,
    ErrorGeneric,
    FailureStopped,
}
impl Language {
    pub fn text(self, key: Text) -> &'static str {
        let pair = match key {
            Text::Copy => ("コピー", "Copy"),
            Text::Restore => ("復元", "Restore"),
            Text::Selected => ("✓ 選択中", "✓ Selected"),
            Text::Switch => ("切り替え", "Switch mode"),
            Text::Locked => ("切り替え不可", "Unavailable"),
            Text::ModeLocked => (
                "処理中はモードを切り替えできません",
                "Mode cannot be changed during processing",
            ),
            Text::SelectSources => ("対象確認", "Select disks"),
            Text::Copying => ("コピー", "Copy"),
            Text::Restoring => ("復元", "Restore"),
            Text::Finish => ("完了", "Complete"),
            Text::CopyTitle => (
                "コピーするディスクを確認してください",
                "Choose the disks to copy",
            ),
            Text::ChangeSources => (
                "変更する場合は［1］～［3］キーを押してください（選択／解除）。",
                "Press [1]–[3] to select or deselect a disk.",
            ),
            Text::Preparing => ("コピーの準備をしています", "Preparing to copy"),
            Text::Ready => ("コピーの準備ができました", "Ready to copy"),
            Text::Countdown => (
                "{seconds}秒後にコピーを開始します",
                "Copy starts in {seconds} seconds",
            ),
            Text::ManualStart => (
                "選択を変更しました。Sキーで開始します。",
                "Selection changed. Press S to start.",
            ),
            Text::NoDisks => ("対象となるディスクがありません", "No eligible disks"),
            Text::NoBackups => (
                "完了したコピーが見つかりません",
                "No completed copies found",
            ),
            Text::RestoreSource => ("復元元選択", "Choose a saved copy"),
            Text::RestoreMapping => (
                "復元するディスクと復元先を選択してください",
                "Select a saved disk and a destination disk",
            ),
            Text::RestoreReview => ("復元する内容を確認してください", "Review the restore"),
            Text::OverwriteWarning => (
                "復元先ディスクの内容はすべて上書きされます。",
                "All contents of the destination disk will be overwritten.",
            ),
            Text::Start => ("開始", "Start"),
            Text::Next => ("次へ", "Next"),
            Text::Back => ("戻る", "Back"),
            Text::Pause => ("一時停止", "Pause"),
            Text::Resume => ("再開", "Resume"),
            Text::Cancel => ("中止", "Stop"),
            Text::Details => ("詳細・内部ログ", "Details / internal log"),
            Text::LogsTitle => ("詳細：内部ログ", "Details: internal log"),
            Text::LogsBack => ("進捗画面へ戻る", "Back to progress"),
            Text::LogsControls => (
                "PgUp：前のページ  PgDn：次のページ  End：最新  I / Esc：戻る",
                "PgUp: older  PgDn: newer  End: latest  I / Esc: back",
            ),
            Text::AbortTitle => ("中止してよろしいですか？", "Stop this operation?"),
            Text::Continue => ("処理を続ける", "Keep processing"),
            Text::Abort => ("中止する", "Stop operation"),
            Text::AbortCopyWarning => (
                "完了済みのコピーは残ります。処理中のコピーは未完了になります。",
                "Completed copies are kept. The current copy will remain incomplete.",
            ),
            Text::AbortRestoreWarning => (
                "復元先のディスクは、書き込み途中の状態で残ります。",
                "The destination disk will be left only partially restored.",
            ),
            Text::Overall => ("全体の進捗", "Overall progress"),
            Text::Eta => ("すべて完了するまでの目安", "Estimated time remaining"),
            Text::Estimating => ("推定中", "Estimating"),
            Text::Read => ("読出し", "Read"),
            Text::Durable => ("保存済み（元データ換算）", "Saved (original data)"),
            Text::Written => ("復元済み", "Written"),
            Text::Confirmed => ("確認済み", "Checked"),
            Text::Total => ("全体", "Total"),
            Text::DoneCount => ("{done} / {total}台完了", "{done} / {total} disks complete"),
            Text::ReadBlock => ("読出ブロック", "Read block"),
            Text::Compression => ("圧縮方式", "Compression"),
            Text::Workers => ("処理並列度", "Copy workers"),
            Text::VerifyWorkers => ("確認並列度", "Check workers"),
            Text::Frame => ("圧縮単位", "Frame"),
            Text::Hash => ("確認", "Check"),
            Text::Destination => ("コピー先", "Destination"),
            Text::Filesystem => ("ファイルシステム", "File system"),
            Text::Free => ("空き", "Free"),
            Text::Done => ("コピー・確認済み", "Copied and checked"),
            Text::Working => ("処理中", "Processing"),
            Text::Waiting => ("待機", "Waiting"),
            Text::Unavailable => ("利用できません", "Unavailable"),
            Text::SourceKeys => (
                "数字キーで選択／解除できます。",
                "Use the number keys to select or deselect.",
            ),
            Text::TargetKeys => (
                "復元元は［1］〜［3］、復元先は［A］［D］［F］で選択。PgUp / PgDn：ページ切り替え。",
                "Source: [1]–[3]. Destination: [A], [D], [F]. PgUp / PgDn: change page.",
            ),
            Text::PageKeys => ("PgUp / PgDn：ページ切り替え", "PgUp / PgDn: change page"),
            Text::AllFinished => ("すべての処理が完了しました", "All operations completed"),
            Text::Shutdown => (
                "あと{seconds}秒でシャットダウンします。",
                "Shutting down in {seconds} seconds.",
            ),
            Text::CopyTail => (
                "正しくコピーできたことを確認しています",
                "Checking that the data was copied correctly",
            ),
            Text::RestoreTail => (
                "正しく復元できたことを確認しています",
                "Checking that the data was restored correctly",
            ),
            Text::PreparationFailed => (
                "準備中に問題が見つかりました",
                "A problem was found during preparation",
            ),
            Text::SelectedCount => (
                "{count}台を選択・合計{capacity}",
                "{count} disks selected · {capacity} total",
            ),
            Text::CurrentDisk => ("{name}を{operation}しています", "{operation}: {name}"),
            Text::UsedBackup => ("使用するコピー", "Saved copy"),
            Text::SourceDisk => ("元のディスク", "Original disk"),
            Text::TargetDisk => ("上書きするディスク", "Disk to overwrite"),
            Text::RestoreStart => ("内容を確認して復元を開始", "Confirm and start restore"),
            Text::WaitingForConfirmation => ("確認を待っています", "Waiting for confirmation"),
            Text::Failure => ("処理中に問題が発生しました", "The operation failed"),
            Text::Cancelled => ("処理を中止しました", "Operation stopped"),
            Text::Poweroff => ("電源を切る準備をしています", "Preparing to shut down"),
            Text::Logging => ("終了処理を行っています", "Finishing up"),
            Text::UsbDisk => ("USBハードディスク", "USB hard disk"),
            Text::ErrorLowSpace => ("保存先の空き容量が1GBを下回ったため、中止しました。", "Stopped because the destination has less than 1 GB of free space."),
            Text::ErrorFull => ("保存先の空き容量が不足しています。処理を中止しました。", "The destination is full. The operation was stopped."),
            Text::ErrorIo => ("ディスクの読み書きに失敗したため、中止しました。", "A disk read or write failed. The operation was stopped."),
            Text::ErrorBootIso => ("起動用ISO「/crispawn.iso」が見つかりません。起動用USBを作り直してください。", "The boot ISO /crispawn.iso was not found. Recreate the boot USB."),
            Text::ErrorDevice => ("必要なディスクが見つからないか、接続が切れました。", "A required disk is missing or was disconnected."),
            Text::ErrorReadOnly => ("保存先に書き込めません。書き込み保護を確認してください。", "The destination is read-only. Check its write protection."),
            Text::ErrorSource => ("対象ディスクの状態が変わったため、安全のため中止しました。", "The source disk changed. The operation was stopped for safety."),
            Text::ErrorConfig => ("設定に問題があるため、処理を開始できません。", "The operation cannot start because the configuration is invalid or unsupported."),
            Text::ErrorVerification => ("保存データの確認で不一致が見つかったため、中止しました。", "Data verification found a mismatch. The operation was stopped."),
            Text::ErrorTimeout => ("ディスクの応答がないため、処理を中止しました。", "The disk stopped responding. The operation was stopped."),
            Text::ErrorMemory => ("処理に必要なメモリが不足しています。", "There is not enough memory for this operation."),
            Text::ErrorGeneric => ("問題が発生したため、処理を中止しました。詳細は内部ログを確認してください。", "The operation stopped because of an error. See the internal log for details."),
            Text::FailureStopped => ("エラーのため停止しています。電源は自動では切れません。", "Stopped after an error. The computer will remain powered on."),
        };
        match self {
            Language::Ja => pair.0,
            Language::En => pair.1,
        }
    }
    /// Display a translated reason while retaining machine codes and raw detail in logs.
    pub fn error_detail(self, detail: &str) -> String {
        let code = detail.split(':').next().unwrap_or("");
        let key = match code {
            "DESTINATION_LOW_SPACE" => Text::ErrorLowSpace,
            "DESTINATION_FULL" | "INSUFFICIENT_SPACE" => Text::ErrorFull,
            "IO_ERROR" | "DIAGNOSTIC_LOG_FAILED" | "NTFS_SYNC_FAILED" => Text::ErrorIo,
            "BOOT_ISO_NOT_FOUND" => Text::ErrorBootIso,
            "DEVICE_UNAVAILABLE"
            | "SOURCE_NOT_FOUND"
            | "BOOT_VOLUME_NOT_FOUND"
            | "DESTINATION_NOT_FOUND" => Text::ErrorDevice,
            "READ_ONLY_DESTINATION" => Text::ErrorReadOnly,
            "SOURCE_CHANGED" | "DEVICE_CHANGED" | "SOURCE_IN_USE" => Text::ErrorSource,
            "INVALID_CONFIG"
            | "UNSUPPORTED_CAPABILITY"
            | "PREPARATION_FAILED"
            | "OPTICAL_DESTINATION_UUID_REQUIRED" => Text::ErrorConfig,
            "HASH_MISMATCH"
            | "DATA_HASH_MISMATCH"
            | "VERIFY_MISMATCH"
            | "MISSING_VERIFICATION"
            | "INVALID_CONTAINER"
            | "UNCOMMITTED_ARCHIVE" => Text::ErrorVerification,
            "IO_TIMEOUT" | "IO_STALLED" | "DIAGNOSTIC_LOG_STALLED" | "SHUTDOWN_STALLED" => {
                Text::ErrorTimeout
            }
            "MEMORY_BUDGET" => Text::ErrorMemory,
            "CANCELLED" => Text::Cancelled,
            _ => Text::ErrorGeneric,
        };
        if !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
        {
            format!("{} [{}]", self.text(key), code)
        } else {
            self.text(key).into()
        }
    }
    /// Substitute only the original template; user/device strings are never
    /// recursively interpreted as placeholders or commands.
    pub fn message(self, key: Text, values: &[(&str, &str)]) -> Result<String> {
        let mut template = self.text(key);
        let mut out = String::new();
        while let Some(at) = template.find('{') {
            out.push_str(&template[..at]);
            let rest = &template[at + 1..];
            let end = rest
                .find('}')
                .ok_or_else(|| Error::new("INVALID_TRANSLATION", "unterminated placeholder"))?;
            let name = &rest[..end];
            let value = values
                .iter()
                .find(|(k, _)| *k == name)
                .ok_or_else(|| Error::new("MISSING_TRANSLATION_VALUE", name))?
                .1;
            out.push_str(value);
            template = &rest[end + 1..];
        }
        out.push_str(template);
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_reasons_are_translated_without_exposing_raw_error_detail() {
        let ja = Language::Ja.error_detail("DESTINATION_LOW_SPACE: private raw filesystem detail");
        assert!(ja.contains("空き容量が1GB"));
        assert!(ja.contains("DESTINATION_LOW_SPACE"));
        assert!(!ja.contains("private raw"));
        assert!(Language::En
            .error_detail("DESTINATION_LOW_SPACE: detail")
            .contains("less than 1 GB"));
        assert!(Language::Ja
            .error_detail("IO_ERROR: English-only OS error")
            .contains("読み書き"));
        assert!(!Language::Ja
            .error_detail("UNKNOWN: secret detail")
            .contains("secret"));
    }
    #[test]
    fn missing_boot_iso_is_named_in_both_languages() {
        for language in [Language::Ja, Language::En] {
            let message = language
                .error_detail("BOOT_ISO_NOT_FOUND: mounted path /run/crispawn/boot/crispawn.iso");
            assert!(message.contains("/crispawn.iso"));
            assert!(message.contains("BOOT_ISO_NOT_FOUND"));
            assert!(!message.contains("mounted path"));
        }
        assert!(Language::Ja
            .error_detail("BOOT_ISO_NOT_FOUND: detail")
            .contains("見つかりません"));
    }
    #[test]
    fn messages_preserve_values_and_do_not_expand_device_text() {
        for language in [Language::Ja, Language::En] {
            let value = language
                .message(
                    Text::CurrentDisk,
                    &[("name", "SSD {operation}"), ("operation", "COPY")],
                )
                .unwrap();
            assert!(value.contains("SSD {operation}"));
            assert!(value.contains("COPY"));
            assert!(language.message(Text::Shutdown, &[]).is_err());
        }
    }
}
