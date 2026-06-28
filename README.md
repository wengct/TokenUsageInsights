# 🌟 Unified AI CLI Token Usage Insights Dashboard

這是一個合併了 **Google Antigravity CLI**、**GitHub Copilot CLI** 與 **Codex CLI** 三個終端 AI 助理的本地 Token 消耗與會話分析綜合看板。

本專案使用高效能的 **Rust (Axum)** 作為後端，搭配 **深色毛玻璃風格 (Glassmorphism)** 前端，協助您在一處集中查看所有本地 AI 助理的 Token 使用量、命中率、對話耗時以及**還原與重建每個會話的完整歷史對話時間軸**！

---

## 🌟 核心特色 (Key Features)

1. **🌟 統一總覽 (Consolidated Overview)**
   * 提供橫跨三大 AI 助理的每日/每月 Token 消耗趨勢與費用對比，客觀分析不同 AI 助理在開發流程中的使用比率與費用開銷。
2. **🔄 策略 B 數據自動遷移 (Auto Migration)**
   * 在啟動時，若偵測到 legacy 的個別資料庫檔案（如 `antigravity_cli_token_insights.db` 等），系統會自動將歷史紀錄遷移至新資料表，並安全備份舊資料庫，確保歷史統計數據不遺失。
3. **⏱️ 三大助理時間軸還原**
   * 整合側邊滑出式抽屜，相容三種日誌格式。流暢還原使用者提示詞、LLM 推理思考過程、CLI 本地工具（Tool Steps）入參、 stdout/stderr 與執行狀態。

---

## ⚙️ 前置作業啟用指南

### 1️⃣ Codex CLI
* **免安裝 Hook**：Codex 採用無侵入式設計，後端會自動遞迴掃描您本地的 `~/.codex/sessions` 目錄，無須進行任何 statusline 或 hook 配置。

### 2️⃣ Google Antigravity CLI
1. 複製收集腳本：
   ```bash
   mkdir -p ~/.gemini/antigravity-cli
   cp shell/antigravity/statusline-token.sh ~/.gemini/antigravity-cli/statusline-token.sh
   chmod +x ~/.gemini/antigravity-cli/statusline-token.sh
   ```
2. 編輯設定檔 `~/.gemini/antigravity-cli/settings.json`：
   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "/home/chenting/.gemini/antigravity-cli/statusline-token.sh",
       "padding": 1
     }
   }
   ```

### 3️⃣ GitHub Copilot CLI
1. 複製收集腳本：
   ```bash
   mkdir -p ~/.copilot
   cp shell/copilot/statusline-token.sh ~/.copilot/statusline-token.sh
   chmod +x ~/.copilot/statusline-token.sh
   ```
2. 編輯設定檔 `~/.copilot/settings.json`：
   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "/home/chenting/.copilot/statusline-token.sh",
       "padding": 1
     }
   }
   ```

---

## 🚀 啟動與常駐服務

### 一、本地啟動
在專案根目錄下執行：
```bash
cargo run
```
在瀏覽器打開 [**`http://localhost:3003`**](http://localhost:3003) 即可使用您的綜合分析看板！

### 二、配置為 Systemd 使用者常駐背景服務
1. 編譯二進位執行檔：
   ```bash
   cargo build --release
   ```
2. 配置服務描述檔：
   ```bash
   mkdir -p ~/.config/systemd/user/
   sed "s|<PROJECT_DIR>|$PWD|g" shell/token-usage-insights.service > ~/.config/systemd/user/token-usage-insights.service
   systemctl --user daemon-reload
   ```
3. 管理服務命令：
   * **啟動服務**：`systemctl --user start token-usage-insights.service`
   * **設定自動啟動**：`systemctl --user enable token-usage-insights.service`
   * **查看日誌**：`journalctl --user -u token-usage-insights.service -n 50 -f`
