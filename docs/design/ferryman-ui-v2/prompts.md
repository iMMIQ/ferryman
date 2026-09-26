# Image generation prompts

Tool: built-in `image_gen` (imagegen skill), new image generation, no reference images.

## Desktop

Use case: ui-mockup.
Create a high-fidelity, implementation-ready DESKTOP web application UI mockup for Ferryman, a Chinese-language local document translation workbench running on Lazycat MicroServer with an AI Pod. This is a new design proposal, not a screenshot of an implemented app. Render a single flat front-facing application screen, approximately 1600 x 1100, with crisp legible simplified Chinese text. No laptop frame, perspective, cinematic lighting or promotional poster.

Visual direction: preserve Ferryman's existing restrained forest-green identity (#176B50), off-white canvas (#F4F6F5), white surfaces, dark charcoal text (#17211D). Thoughtful editorial typography, clean CJK sans-serif, generous but efficient spacing, fine dividers, 8–12px corner radii, subtle elevation only for the main panel. Excellent contrast; small text stays readable. Minimal consistent outline icons. Avoid decorative charts, gradients, glassmorphism, huge empty areas, meaningless KPIs or redundant cards.

Information architecture:
A quiet 200px left sidebar: compact square F brand and "Ferryman", small descriptor "文档翻译"; selected navigation "新建翻译", secondary "翻译任务" with small badge "3", then separated "模型与算力". At bottom a small green-dot "算力舱已连接" and muted "模型就绪". No invented cloud subscriptions, chat, team sharing or billing.

Main workspace takes remaining width with 36px outer margins. Top title "新建翻译" and subtitle "文档、电子书与字幕，一处翻译。" On the far right a compact chip "Hy-MT2 · 7B" and text link "管理模型". Runtime controls should not dominate.

Main layout: a wide translation composer, about 740px, with a narrower 310px right column of recent tasks.
Composer white panel:
1. Clear section heading "1  选择文件"; a compact segmented control "上传文件" selected and "文稿与网盘". A moderately sized pale-green dashed drop zone with simple upload icon, text "拖入文件，或点击选择", secondary "EPUB · DOCX · PDF · 字幕 · TXT · Markdown", and "支持多文件 · 单文件最大 512 MB". Beneath show two selected file rows, one "Design Systems.epub" with "2.4 MB", one "Product Brief.pdf" with "840 KB", recognizable small file-type icons, unobtrusive remove buttons. Both comfortably fit with the form.
2. Divider and heading "2  翻译设置". Aligned two-column controls: "目标语言" with dropdown "简体中文"; "翻译模型" with selector "Hy-MT2 7B" and restrained "速度优先" descriptor. Beneath, labeled "输出方式" with two accessible horizontal radio tiles "双语对照" selected, secondary "保留原文，逐段对照"; "仅译文", secondary "专注译后阅读". No source language selector needed, no invented automatic quality metric.
3. Single compact output row "保存位置" -> "完成后下载" with a folder icon and "更改". A clearly named collapsed disclosure "高级设置", with quiet summary "批量 25 段 · 上下文 5 段".
Composer footer with thin divider: a brief summary on the left "2 个文件 · 简体中文 · 双语对照" and one strong forest-green primary button "开始翻译  →" on the right. Under summary a small reassurance "原文件不会被修改". Avoid extra competing primary buttons.

Right column: heading "最近任务", quiet link "查看全部".
A focused current-task block for "The Design of Everyday Things.epub": small green "翻译中" label, thin green progress bar at 35%, explicit "148 / 420 段" and "35%", quiet "查看详情" and "取消" actions.
A completed task row "产品使用手册.pdf", green check with text "已完成", metadata "双语对照 · 中文", visible secondary "下载结果" button.
A failed task row "字幕合集.srt", amber/red warning icon and explicit "连接中断", one-line actionable helper "检查算力舱连接后重试", secondary "重试". No tiny clipped errors, no color-only statuses.
Below these a soft muted explanation "任务在后台继续，离开页面也不会中断。"

Design must feel like a calm, refined, useful shipping product. Keep every action legible and align all controls to a coherent 8px spacing grid. Use actual Chinese labels above accurately. Focus on hierarchy and one clear next action. This is a finished UI mockup, not annotated wireframes. No watermarks, external brands or fake browser chrome.

## Mobile

Use case: ui-mockup.
Design the MOBILE companion to a refined Ferryman document-translation web app. Render a high-resolution design board containing TWO flat mobile viewport mockups side-by-side, each approx 430px wide and 920px tall, separated by a clean gutter on an off-white board. No device frames or perspective. Tiny outside captions only: "新建翻译" above the left screen and "任务进度" above the right screen. Chinese UI typography must be crisp and legible.

Maintain forest green #176B50, off-white #F4F6F5, white panels, charcoal #17211D, thin gray-green borders, 10px radii, understated shadows. Professional, calm, polished, accessible. No charts, glassmorphism, purple gradients, decorative illustrations, giant hero sections or unsupported cloud/team/chat functionality.

Both screens have compact header with square F mark, "Ferryman", and small green-dot status chip "已连接"; a consistent bottom navigation with "新建翻译", "任务" with badge 3, and "模型". Make 44px touch targets and enough safe-area padding.

LEFT screen = ready-to-submit state.
Heading "新建翻译", small subtitle "文档、电子书与字幕，一处翻译。"
Section "选择文件" with segmented "上传文件" selected / "文稿与网盘".
A compact outlined upload zone: upload icon, "点击选择文件", smaller "支持 PDF、EPUB、DOCX 等".
Selected file rows: "Design Systems.epub" with "2.4 MB", and "Product Brief.pdf" with "840 KB", file icons and remove controls.
Section "翻译设置": "目标语言" value "简体中文"; "模型" value "Hy-MT2 7B" with green "就绪" text. A segmented output selector "双语对照" selected / "仅译文".
Compact "保存位置" value "完成后下载" chevron.
Collapsed row "高级设置" with chevron.
Sticky action area ABOVE bottom nav, with small summary "2 个文件 · 原文件不会被修改" and full-width green button "开始翻译". Ensure sticky area doesn't hide any form field. Left bottom-nav item is active.

RIGHT screen = task management.
Heading "翻译任务", compact count "1 个进行中".
Filter chips "全部" selected, "进行中", "已完成", "失败".
A generous current-task card with EPUB icon and fully readable wrapped filename "The Design of Everyday Things.epub", green status pill "翻译中"; progress bar 35%; "148 / 420 段" left and "35%" right; metadata "简体中文 · 双语对照"; quiet outlined "查看详情" and text "取消" actions. No unsupported pause button.
A completed row/card "产品使用手册.pdf", green check "已完成", "简体中文 · 双语对照", obvious outlined "下载结果" action.
A failed row/card "字幕合集.srt", warning icon "连接中断", readable message "检查算力舱连接后重试", outlined "重试" action. Do not rely on color alone.
Small text beneath "任务在后台继续，离开页面也不会中断。"
Right bottom-nav "任务" is active.

These are practical implementation-ready UI designs, consistent with the desktop design language. Prioritize scanning, explicit states, easy recovery and clear primary actions. No watermark, browser toolbar, external branding, invented model-cost estimates or redundant controls.

