#!/usr/bin/env python3
"""Preserve and audit exact source-line coverage; never modify the original prompt.

Run from any directory: python docs/requirements/split-and-verify.py --check
Use --write only to regenerate the exact module copies, ledger, and manifest.
No network access and no paths outside docs/requirements are written.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SOURCE = ROOT / "000-original-user-prompt.md"
# Inclusive, one-based ranges. Every original line (including blanks) appears once.
MODULES = [
    ("001-positioning", 1, 15, "中文定位、环境授权和三项产品重点"),
    ("002-mission", 16, 36, "1. 产品使命与完整生态"),
    ("003-research", 37, 91, "2. 合法竞品研究与20项功能矩阵"),
    ("004-platforms", 92, 110, "3. 四平台主控、被控与Linux中继"),
    ("005-core-introduction", 111, 112, "4. 核心功能标题"),
    ("006-virtual-display", 113, 159, "4.1 虚拟显示与平台原生实现"),
    ("007-input", 160, 193, "4.2 键鼠、国际布局和快捷键映射"),
    ("008-clipboard", 194, 221, "4.3 剪贴板、权限和逐连接配置"),
    ("009-streaming", 222, 259, "4.4 编解码、自适应和连接统计"),
    ("010-networking", 260, 293, "5. P2P、自动回退和用户选定中继"),
    ("011-relay-server", 294, 325, "6. 独立Linux中继服务"),
    ("012-relay-deployment", 326, 354, "6.1 极简安装、维护和诊断"),
    ("013-relay-profiles", 355, 384, "6.2 不限数量的中继配置"),
    ("014-relay-selection", 385, 410, "6.3 质量选择、故障切换与分布式演进"),
    ("015-android", 411, 435, "7. Android原生体验和权限限制"),
    ("016-security", 436, 464, "8. E2E、认证、授权和安全更新"),
    ("017-ui", 465, 512, "9. 主界面、连接界面和中继管理"),
    ("018-additional-features", 513, 539, "10. 需评估的20项扩展功能"),
    ("019-architecture", 540, 579, "11. 架构候选、17模块和稳定接口"),
    ("020-development-phases", 580, 642, "12. 自主开发与五阶段交付"),
    ("021-testing", 643, 683, "13. 真实平台组合、网络路径和故障测试"),
    ("022-release", 684, 716, "14. GitHub、开源合规与发布物"),
    ("023-engineering", 717, 738, "15. 15项强制工程原则与真实限制"),
    ("024-final-objective", 739, 764, "16. 最终目标及完成判据"),
    ("025-extra-decisions-introduction", 765, 768, "三项额外设计决策标题"),
    ("026-decision-session-migration", 769, 796, "补充1：质量检测、中继池与可恢复会话迁移"),
    ("027-decision-virtual-display", 797, 810, "补充2：独立虚拟显示核心模块"),
    ("028-decision-relay-modes", 811, 825, "补充3：个人自建优先与多服务器管理"),
]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def kind(text: str) -> str:
    if not text:
        return "blank"
    if text.startswith("#"):
        return "heading"
    if text.startswith("|"):
        return "table-row"
    if text.startswith("- ") or (text[:1].isdigit() and ". " in text[:5]):
        return "list-item"
    return "paragraph"


def expected_files() -> tuple[dict[Path, bytes], dict]:
    original = SOURCE.read_bytes()
    lines = original.splitlines(keepends=True)
    if len(lines) != 825:
        raise ValueError(f"Frozen source changed: expected 825 lines, got {len(lines)}")
    text_lines = [line.decode("utf-8").rstrip("\r\n") for line in lines]
    source_hash = sha256(original)
    files: dict[Path, bytes] = {}
    entries = []
    ledger = []
    seen = []
    for name, start, end, title in MODULES:
        data = b"".join(lines[start - 1:end])
        rel = f"modules/{name}.md"
        files[ROOT / rel] = data
        entries.append({"file": rel, "title": title, "startLine": start,
                        "endLine": end, "lineCount": end - start + 1,
                        "sha256": sha256(data)})
        for number in range(start, end + 1):
            text = text_lines[number - 1]
            seen.append(number)
            ledger.append({"id": f"L{number:04d}", "sourceLine": number,
                           "module": rel, "kind": kind(text), "text": text})
    if seen != list(range(1, len(lines) + 1)):
        raise ValueError("Source coverage has omissions, overlaps, or reordering")
    if b"".join(files.values()) != original:
        raise ValueError("Concatenated module bytes do not equal original")
    decisions = [
        {"name": "recoverable-session-migration", "sourceLines": [769, 796],
         "requiredExactText": "特别注意：P2P 和 Relay 之间的切换应该设计成可恢复的会话迁移，而不只是断开后重新连接。"},
        {"name": "independent-virtual-display-core", "sourceLines": [797, 810],
         "requiredExactText": "### **2. 虚拟屏必须成为独立的核心模块**"},
        {"name": "personal-and-multi-server-relay", "sourceLines": [811, 825],
         "requiredExactText": "### **3. 中继服务器最好支持两种使用模式**"},
    ]
    for decision in decisions:
        start, end = decision["sourceLines"]
        if decision["requiredExactText"] not in text_lines[start - 1:end]:
            raise ValueError(f"Missing extra decision: {decision['name']}")
    paragraph_blocks = 0
    in_block = False
    for text in text_lines:
        if text and not in_block:
            paragraph_blocks += 1
        in_block = bool(text)
    manifest = {"source": SOURCE.name, "encoding": "UTF-8",
                "sourceSha256": source_hash, "sourceByteCount": len(original),
                "sourceLineCount": len(lines),
                "nonblankLineCount": sum(bool(line) for line in text_lines),
                "nonblankBlocks": paragraph_blocks,
                "moduleCount": len(MODULES), "coveredLineCount": len(seen),
                "missingLines": [], "duplicatedLines": [],
                "byteExactConcatenation": True, "modules": entries,
                "extraDecisions": decisions}
    files[ROOT / "coverage-manifest.json"] = (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    files[ROOT / "requirements-ledger.jsonl"] = ("\n".join(json.dumps(row, ensure_ascii=False) for row in ledger) + "\n").encode("utf-8")
    rows = ["# 原文逐行覆盖校验", "", "此报告验证需求持久化的完整性，不代表任何产品功能已实现或验收。", "",
            f"- 原文：`{SOURCE.name}`，UTF-8，共 {len(lines)} 行、{len(original)} 字节。",
            f"- SHA-256：`{source_hash}`。",
            f"- 非空行 {manifest['nonblankLineCount']}；连续非空段块 {paragraph_blocks}；模块 {len(MODULES)}。",
            "- 覆盖所有行（包括空行），缺失 0，重复 0，顺序相同。",
            "- 按清单顺序连接全部模块的原始字节，必须与原文逐字节相同。",
            "- 三项补充决策分别保留在模块026、027、028，并校验关键原文。",
            "- `requirements-ledger.jsonl` 为每行保留稳定的 L0001–L0825 编号、类型、原文字段和模块位置；编号表示需求来源，不是实现状态。", "",
            "复核：`python docs/requirements/split-and-verify.py --check`。", "",
            "| 模块 | 原文行（含首尾） | 行数 |", "| --- | --- | ---: |"]
    rows.extend(f"| [{entry['title']}]({entry['file']}) | {entry['startLine']}–{entry['endLine']} | {entry['lineCount']} |" for entry in entries)
    files[ROOT / "COVERAGE.md"] = ("\n".join(rows) + "\n").encode("utf-8")
    return files, manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    args = parser.parse_args()
    before = SOURCE.read_bytes()
    files, manifest = expected_files()
    if args.write:
        for path, data in files.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
    mismatches = [str(path.relative_to(ROOT)) for path, data in files.items()
                  if not path.is_file() or path.read_bytes() != data]
    if SOURCE.read_bytes() != before:
        raise SystemExit("FAIL: original prompt changed during operation")
    if mismatches:
        raise SystemExit("FAIL: missing or changed generated files: " + ", ".join(mismatches))
    print(json.dumps({"result": "PASS", "mode": "write-and-check" if args.write else "check-only",
                      "sourceSha256": manifest["sourceSha256"],
                      "sourceLines": manifest["sourceLineCount"],
                      "nonblankLines": manifest["nonblankLineCount"],
                      "nonblankBlocks": manifest["nonblankBlocks"],
                      "modules": manifest["moduleCount"], "missingLines": 0,
                      "duplicatedLines": 0, "byteExactConcatenation": True,
                      "extraDecisions": 3}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
