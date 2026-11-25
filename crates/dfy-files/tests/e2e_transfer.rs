// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 端到端测试：真实临时目录 + 两个状态机直接对接（不经过网络）。
//!
//! 路由规则与 `dfy-session::CtrlHandler` 里完全一致：
//! `FileChunk → receiver.on_chunk()`、`FileAck → sender.on_ack()`、
//! `FileResult → sender.on_result()`；接收端产出的回包同步喂回发送端，
//! 因此也顺带验证了「ack 节流 / 在途窗口」不会死锁。

use std::fs;

use dfy_files::{
    plan_directory, FileError, FileReceiver, FileSender, OverwritePolicy, ReceiveConfig,
    SenderConfig,
};
use dfy_proto::envelope::Body;
use dfy_proto::{Envelope, FileChunk, FileOffer};
use tempfile::tempdir;

const CHUNK: usize = 256 * 1024;
/// 协议里的偏移/大小都是 `u64`，这里也用 `u64` 便于直接比较。
const MIB: u64 = 1024 * 1024;

/// 确定性伪随机内容（避免全 0 掩盖 offset 错误）。
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

/// 一轮投递的统计。
#[derive(Debug, Default)]
struct PumpOutcome {
    acks: usize,
    results: usize,
}

/// 把一个信封投递给接收端，并把回包喂回发送端。
///
/// `block_index` 仅用于故障注入测试（按块序号翻转首字节）。
fn route(
    sender: &mut FileSender,
    receiver: &mut FileReceiver,
    env: Envelope,
    block_index: usize,
    mutate_block: Option<usize>,
) -> Result<PumpOutcome, FileError> {
    let Some(Body::FileChunk(mut chunk)) = env.body else {
        panic!("发送端只应产出 FileChunk");
    };
    if mutate_block == Some(block_index) && !chunk.data.is_empty() {
        // 模拟链路/磁盘损坏：内容变了但长度不变，最终应被 BLAKE3 抓住。
        chunk.data[0] ^= 0xFF;
    }
    let mut outcome = PumpOutcome::default();
    for out in receiver.on_chunk(&chunk)? {
        match out.body {
            Some(Body::FileAck(ack)) => {
                outcome.acks += 1;
                sender.on_ack(&ack);
            }
            Some(Body::FileResult(result)) => {
                outcome.results += 1;
                sender.on_result(&result)?;
            }
            other => panic!("接收端不应产出 {other:?}"),
        }
    }
    Ok(outcome)
}

/// 把当前传输从 `next_chunk` 一直泵到 `on_result`。
fn pump(
    sender: &mut FileSender,
    receiver: &mut FileReceiver,
    mutate_block: Option<usize>,
) -> Result<PumpOutcome, FileError> {
    let mut total = PumpOutcome::default();
    let mut block_index = 0usize;
    loop {
        if sender.is_complete() || sender.is_rejected() {
            break;
        }
        let Some(env) = sender.next_chunk(CHUNK)? else {
            assert!(
                !sender.is_blocked_on_ack(),
                "pump 死锁：在途窗口满但接收端没有产出 ack（ack 阈值与窗口配置不匹配）"
            );
            break;
        };
        let one = route(sender, receiver, env, block_index, mutate_block)?;
        total.acks += one.acks;
        total.results += one.results;
        block_index += 1;
    }
    Ok(total)
}

fn offer_of(transfer_id: u64, name: &str, size: u64, is_dir: bool) -> FileOffer {
    FileOffer {
        transfer_id,
        name: name.to_string(),
        size,
        last_modified_ms: 0,
        is_dir,
    }
}

#[test]
fn small_file_one_kib_end_to_end() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    let data = pattern(1024);
    let src = src_root.join("小文件 1KB.bin");
    fs::write(&src, &data).unwrap();

    let mut sender = FileSender::new(100);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Skip);

    let offer = sender.offer(&src).unwrap();
    assert_eq!(offer.name, "小文件 1KB.bin");
    assert_eq!(offer.size, 1024);
    assert_eq!(offer.transfer_id, 100);

    let (accept, target) = receiver.on_offer(&offer).unwrap();
    assert!(accept.accept);
    assert_eq!(accept.resume_offset, 0);
    assert!(sender.on_accept(&accept).unwrap().is_empty());

    let outcome = pump(&mut sender, &mut receiver, None).unwrap();
    assert!(sender.is_complete(), "发送端应收到成功 FileResult");
    assert_eq!(outcome.results, 1);
    assert_eq!(outcome.acks, 1, "小文件收尾时回一次 ack");

    // 内容逐字节一致、摘要一致。
    assert_eq!(fs::read(&target).unwrap(), data);
    assert!(target.starts_with(tmp.path().join("recv")));
    assert_eq!(receiver.stats().digest, sender.stats().digest);
    assert!(receiver.stats().digest.is_some());
    // 原子落盘：part 不残留。
    assert!(!recv_root.join("小文件 1KB.bin.dfy-part").exists());
    assert_eq!(sender.stats().bytes_sent, 1024);
    assert_eq!(receiver.stats().files_completed, 1);
}

#[test]
fn large_eight_mib_file_resumes_from_four_mib() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    fs::create_dir_all(&recv_root).unwrap();

    let data = pattern((8 * MIB) as usize);
    let src = src_root.join("big.bin");
    fs::write(&src, &data).unwrap();
    // 模拟上次传输中断：接收端目标文件已有前 4 MiB（Overwrite 策略把它当前缀续传）。
    fs::write(recv_root.join("big.bin"), &data[..(4 * MIB) as usize]).unwrap();

    let mut sender = FileSender::new(1);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Overwrite);

    let offer = sender.offer(&src).unwrap();
    let (accept, target) = receiver.on_offer(&offer).unwrap();
    assert!(accept.accept);
    assert_eq!(accept.resume_offset, 4 * MIB, "接收端以已落盘长度为准");
    sender.on_accept(&accept).unwrap();
    assert_eq!(sender.stats().resumed_from, 4 * MIB);

    let outcome = pump(&mut sender, &mut receiver, None).unwrap();
    assert!(sender.is_complete());
    assert_eq!(outcome.results, 1);
    // 只发后半段，接收端拼出的完整内容必须正确。
    assert_eq!(sender.stats().bytes_sent, 4 * MIB);
    assert_eq!(fs::read(&target).unwrap(), data);
    assert_eq!(sender.stats().digest, receiver.stats().digest);
    assert_eq!(receiver.stats().resumed_from, 4 * MIB);
    assert!(!recv_root.join("big.bin.dfy-part").exists());
}

#[test]
fn resume_offset_equal_to_size_sends_empty_last_block() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    fs::create_dir_all(&recv_root).unwrap();
    let data = pattern(4096);
    let src = src_root.join("done.bin");
    fs::write(&src, &data).unwrap();
    // 接收端已经有完整同内容文件：accept 的 resume_offset 会等于 size。
    fs::write(recv_root.join("done.bin"), &data).unwrap();

    let mut sender = FileSender::new(2);
    let mut receiver = FileReceiver::new(recv_root, OverwritePolicy::Overwrite);
    let offer = sender.offer(&src).unwrap();
    let (accept, _target) = receiver.on_offer(&offer).unwrap();
    assert_eq!(accept.resume_offset, data.len() as u64);

    let extra = sender.on_accept(&accept).unwrap();
    assert_eq!(extra.len(), 1, "已完整落盘时应立刻补一个空 last 块");
    let outcome = route(&mut sender, &mut receiver, extra[0].clone(), 0, None).unwrap();
    assert_eq!(outcome.results, 1);
    assert!(sender.is_complete());
    assert_eq!(sender.stats().bytes_sent, 0);
    assert_eq!(sender.stats().chunks_sent, 1);
}

#[test]
fn directory_plan_three_levels_with_empty_dirs_and_chinese() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("tree");
    fs::create_dir_all(root.join("空目录")).unwrap();
    fs::create_dir_all(root.join("层1/层2/层3")).unwrap();
    fs::write(root.join("层1/文件.txt"), "你好，大肥鱼").unwrap();
    fs::write(root.join("层1/层2/层3/deep.bin"), pattern(64)).unwrap();
    fs::write(root.join("根文件.txt"), "root").unwrap();

    let plan = plan_directory(&root).unwrap();
    let relatives: Vec<&str> = plan.iter().map(|e| e.relative.as_str()).collect();
    assert_eq!(
        relatives,
        vec![
            "层1",
            "层1/层2",
            "层1/层2/层3",
            "层1/层2/层3/deep.bin",
            "层1/文件.txt",
            "根文件.txt",
            "空目录",
        ]
    );
    // 空目录保留为目录项、大小为 0。
    let empty = plan.iter().find(|e| e.relative == "空目录").unwrap();
    assert!(empty.is_dir);
    assert_eq!(empty.size, 0);
    // 确定性：两次展开的相对路径/大小/类型一致（mtime 属文件系统元数据，不参与比较）。
    let projection = |entries: &[dfy_files::DirEntryPlan]| -> Vec<(String, u64, bool)> {
        entries
            .iter()
            .map(|e| (e.relative.clone(), e.size, e.is_dir))
            .collect()
    };
    assert_eq!(
        projection(&plan),
        projection(&plan_directory(&root).unwrap())
    );
}

#[test]
fn receiver_rejects_unsafe_paths_and_stays_in_root() {
    let tmp = tempdir().unwrap();
    let recv_root = tmp.path().join("recv");
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Skip);

    for bad in [
        "../../evil",
        r"..\evil",
        "/etc/passwd",
        r"C:\Windows\evil",
        "aux.txt",
        "CON",
        "a/b/../../../evil",
        "file.txt:stream",
    ] {
        let err = receiver.on_offer(&offer_of(1, bad, 8, false)).unwrap_err();
        assert!(
            matches!(err, FileError::InvalidPath(_)),
            "{bad:?} 必须被拒绝, 实际 {err:?}"
        );
    }
    // 接收根下不应产生任何文件。
    assert_eq!(fs::read_dir(&recv_root).map(|d| d.count()).unwrap_or(0), 0);

    // 合法中文相对路径（子目录）可用，且落在根内。
    let (accept, target) = receiver
        .on_offer(&offer_of(2, "子目录/中文名.txt", 0, false))
        .unwrap();
    assert!(accept.accept);
    assert!(target.starts_with(&recv_root));
    assert_eq!(target, recv_root.join("子目录").join("中文名.txt"));
}

#[test]
fn duplicate_chunk_dropped_and_gap_rejected_at_envelope_level() {
    let tmp = tempdir().unwrap();
    let mut receiver = FileReceiver::new(tmp.path().join("recv"), OverwritePolicy::Skip);
    receiver.on_offer(&offer_of(1, "d.bin", 8, false)).unwrap();

    let first = Envelope::file_chunk(1, 0, b"aaaa".to_vec(), false);
    let Some(Body::FileChunk(chunk)) = first.body else {
        unreachable!()
    };
    receiver.on_chunk(&chunk).unwrap();

    // 完全重复：幂等丢弃（ack 丢失后的重发），不回包、不推进。
    let dup = FileChunk {
        transfer_id: 1,
        offset: 0,
        data: b"aaaa".to_vec(),
        last: false,
    };
    assert!(receiver.on_chunk(&dup).unwrap().is_empty());

    // 缺口（期望 4，收到 6）：报 ChunkOutOfOrder 且状态保持可继续。
    let gap = FileChunk {
        transfer_id: 1,
        offset: 6,
        data: b"xx".to_vec(),
        last: false,
    };
    let err = receiver.on_chunk(&gap).unwrap_err();
    assert!(matches!(
        err,
        FileError::ChunkOutOfOrder {
            expected: 4,
            got: 6
        }
    ));

    let tail = FileChunk {
        transfer_id: 1,
        offset: 4,
        data: b"bbbb".to_vec(),
        last: true,
    };
    let out = receiver.on_chunk(&tail).unwrap();
    assert_eq!(out.len(), 2, "最后一块返回 ack + result");
}

#[test]
fn overwrite_skip_and_rename_policies() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    fs::create_dir_all(&recv_root).unwrap();

    let data = pattern(2048);
    let src = src_root.join("photo.jpg");
    fs::write(&src, &data).unwrap();

    // —— Skip：已存在 → 拒绝，原文件不动。
    fs::write(recv_root.join("photo.jpg"), b"old").unwrap();
    let mut sender = FileSender::new(1);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Skip);
    let offer = sender.offer(&src).unwrap();
    let (accept, target) = receiver.on_offer(&offer).unwrap();
    assert!(!accept.accept);
    assert_eq!(target, recv_root.join("photo.jpg"));
    sender.on_accept(&accept).unwrap();
    assert!(sender.is_rejected());
    assert_eq!(fs::read(recv_root.join("photo.jpg")).unwrap(), b"old");

    // —— Overwrite：按已有长度续传（这里旧文件恰好是新文件的前缀），最终整体一致。
    fs::write(recv_root.join("photo.jpg"), &data[..1024]).unwrap();
    let mut sender = FileSender::new(2);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Overwrite);
    let offer = sender.offer(&src).unwrap();
    let (accept, target) = receiver.on_offer(&offer).unwrap();
    assert!(accept.accept);
    assert_eq!(accept.resume_offset, 1024);
    sender.on_accept(&accept).unwrap();
    pump(&mut sender, &mut receiver, None).unwrap();
    assert!(sender.is_complete());
    assert_eq!(fs::read(&target).unwrap(), data);

    // —— Rename：既保留旧文件，也收下新文件。
    fs::write(recv_root.join("photo.jpg"), b"old").unwrap();
    let mut sender = FileSender::new(3);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Rename);
    let offer = sender.offer(&src).unwrap();
    let (accept, target) = receiver.on_offer(&offer).unwrap();
    assert!(accept.accept);
    assert_eq!(target, recv_root.join("photo (1).jpg"));
    sender.on_accept(&accept).unwrap();
    pump(&mut sender, &mut receiver, None).unwrap();
    assert_eq!(fs::read(recv_root.join("photo.jpg")).unwrap(), b"old");
    assert_eq!(fs::read(recv_root.join("photo (1).jpg")).unwrap(), data);
}

#[test]
fn ack_is_throttled_every_two_chunks() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    let data = pattern(6 * 4);
    let src = src_root.join("tiny.bin");
    fs::write(&src, &data).unwrap();

    let mut sender = FileSender::with_config(
        1,
        SenderConfig {
            max_chunk_bytes: 4,
            in_flight_bytes: 16 * 1024,
        },
    );
    let mut receiver = FileReceiver::with_config(
        recv_root,
        OverwritePolicy::Skip,
        ReceiveConfig {
            ack_bytes: u64::MAX,
            ack_chunks: 2,
            max_chunk_bytes: 4,
            max_file_bytes: u64::MAX,
        },
    );
    let offer = sender.offer(&src).unwrap();
    let (accept, _target) = receiver.on_offer(&offer).unwrap();
    sender.on_accept(&accept).unwrap();

    let outcome = pump(&mut sender, &mut receiver, None).unwrap();
    assert!(sender.is_complete());
    // 6 块：块 2、块 4 各一次 ack，最后一块再补一次 = 3。
    assert_eq!(outcome.acks, 3);
    assert_eq!(outcome.results, 1);
}

#[test]
fn sender_window_backpressure_with_dense_acks() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    let data = pattern(64);
    let src = src_root.join("window.bin");
    fs::write(&src, &data).unwrap();

    let mut sender = FileSender::with_config(
        1,
        SenderConfig {
            max_chunk_bytes: 8,
            in_flight_bytes: 8, // 只允许 1 块在途
        },
    );
    let mut receiver = FileReceiver::with_config(
        recv_root,
        OverwritePolicy::Skip,
        ReceiveConfig {
            ack_bytes: 1,
            ack_chunks: 1, // 每块都 ack，窗口才能持续前进
            max_chunk_bytes: 8,
            max_file_bytes: u64::MAX,
        },
    );
    let offer = sender.offer(&src).unwrap();
    let (accept, _target) = receiver.on_offer(&offer).unwrap();
    sender.on_accept(&accept).unwrap();
    let outcome = pump(&mut sender, &mut receiver, None).unwrap();
    assert!(sender.is_complete());
    assert_eq!(outcome.results, 1);
    assert_eq!(sender.stats().bytes_sent, 64);
}

#[test]
fn corrupted_chunk_is_caught_by_blake3() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    let data = pattern(1024);
    let src = src_root.join("corrupt.bin");
    fs::write(&src, &data).unwrap();

    let mut sender = FileSender::new(1);
    let mut receiver = FileReceiver::new(recv_root, OverwritePolicy::Skip);
    let offer = sender.offer(&src).unwrap();
    let (accept, _target) = receiver.on_offer(&offer).unwrap();
    sender.on_accept(&accept).unwrap();

    let err = pump(&mut sender, &mut receiver, Some(0)).unwrap_err();
    assert!(
        matches!(err, FileError::DigestMismatch { .. }),
        "内容损坏必须被摘要校验抓住, 实际 {err:?}"
    );
    assert!(sender.is_complete());
    assert_ne!(sender.stats().digest, receiver.stats().digest);
}

#[test]
fn directory_end_to_end_restores_empty_dirs_and_chinese_names() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src").join("照片");
    fs::create_dir_all(src_root.join("空目录")).unwrap();
    fs::create_dir_all(src_root.join("子/空2")).unwrap();
    fs::write(src_root.join("a.jpg"), pattern(1024)).unwrap();
    fs::write(src_root.join("子/中文.txt"), "你好，世界").unwrap();
    let recv_root = tmp.path().join("recv");

    let mut sender = FileSender::new(1000);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Skip);

    let dir_offer = sender.offer(&src_root).unwrap();
    assert!(dir_offer.is_dir);
    assert_eq!(dir_offer.name, "照片");
    assert_eq!(dir_offer.size, 0);
    let (accept, dir_target) = receiver.on_offer(&dir_offer).unwrap();
    assert!(accept.accept);
    assert!(dir_target.is_dir());
    sender.on_accept(&accept).unwrap();

    let mut files = 0usize;
    while let Some(item) = sender.next_offer().unwrap() {
        let (acc, target) = receiver.on_offer(&item).unwrap();
        assert!(acc.accept, "{} 应被接受", item.name);
        sender.on_accept(&acc).unwrap();
        if item.is_dir {
            assert!(target.is_dir(), "{} 应建成目录", item.name);
            continue;
        }
        pump(&mut sender, &mut receiver, None).unwrap();
        assert!(sender.is_complete(), "{} 应完成", item.name);
        files += 1;
    }
    assert_eq!(files, 2);

    // 目录树还原：空目录存在、中文名与内容一致。
    assert!(recv_root.join("照片/空目录").is_dir());
    assert!(recv_root.join("照片/子/空2").is_dir());
    assert_eq!(
        fs::read(recv_root.join("照片/a.jpg")).unwrap(),
        pattern(1024)
    );
    assert_eq!(
        fs::read(recv_root.join("照片/子/中文.txt")).unwrap(),
        "你好，世界".as_bytes()
    );
    assert_eq!(receiver.stats().files_completed, 2);
    assert!(receiver.stats().dirs_created >= 3);
}

#[test]
fn interrupted_part_file_resumes_without_recounting() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    fs::create_dir_all(&recv_root).unwrap();

    let data = pattern(10 * 1024);
    let src = src_root.join("resume-part.bin");
    fs::write(&src, &data).unwrap();
    // ??????????:??? part ??(?????),Skip ????????
    fs::write(
        recv_root.join("resume-part.bin.dfy-part"),
        &data[..3 * 1024],
    )
    .unwrap();

    let mut sender = FileSender::new(42);
    let mut receiver = FileReceiver::new(recv_root.clone(), OverwritePolicy::Skip);
    let offer = sender.offer(&src).unwrap();
    let (accept, target) = receiver.on_offer(&offer).unwrap();
    assert!(accept.accept);
    assert_eq!(accept.resume_offset, 3 * 1024, "? part ???????");
    assert_eq!(receiver.stats().resumed_from, 3 * 1024);
    sender.on_accept(&accept).unwrap();
    assert_eq!(sender.stats().resumed_from, 3 * 1024);

    let outcome = pump(&mut sender, &mut receiver, None).unwrap();
    assert!(sender.is_complete());
    assert_eq!(outcome.results, 1);
    // ??????,??????????
    assert_eq!(sender.stats().bytes_sent, (10 * 1024 - 3 * 1024) as u64);
    assert_eq!(sender.stats().digest, receiver.stats().digest);
    assert_eq!(fs::read(&target).unwrap(), data);
    assert!(!recv_root.join("resume-part.bin.dfy-part").exists());
}

#[test]
fn oversized_offer_rejected_by_receiver_cap() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("src");
    let recv_root = tmp.path().join("recv");
    fs::create_dir_all(&src_root).unwrap();
    let src = src_root.join("big.bin");
    fs::write(&src, pattern(64)).unwrap();

    let mut sender = FileSender::new(1);
    let mut receiver = FileReceiver::with_config(
        recv_root.clone(),
        OverwritePolicy::Skip,
        ReceiveConfig {
            max_file_bytes: 16,
            ..ReceiveConfig::default()
        },
    );
    let offer = sender.offer(&src).unwrap();
    let err = receiver.on_offer(&offer).unwrap_err();
    assert!(matches!(
        err,
        FileError::FileTooLarge {
            declared: 64,
            max: 16
        }
    ));
    assert!(!receiver.is_receiving());
    assert!(!recv_root.join("big.bin.dfy-part").exists());
}
