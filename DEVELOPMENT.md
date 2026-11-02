# 開發者文件

玩家請看 [README.md](README.md)。這份寫給想改程式、除錯或了解原理的人。

## 原理

不需要 DOSBox。直接執行原版的 16 位元實模式機器碼（`REKO3.COM` → `SBOPL2` / `GRPDRV` / `TFDED` 常駐程式 → `MAIN.EXE`），
它們用到的 DOS、BIOS、VGA、OPL2（AdLib）改由 Rust 實作。遊戲邏輯、劇情、戰鬥、AI、存檔、中文字型與音樂驅動全部都是原版程式碼在跑。

做法沿用《孔明傳》的 koumei-rt（跑原版碼 + 只實作作業系統介面），CPU 直譯器也是從那邊改成實模式的。

```
REKO3.COM（原版啟動器：EXEC 三個常駐程式，再依回傳碼在 OPEN / MAIN / END 之間切換）
  └─ cpu.rs      x86 直譯器（實模式 + 遊戲用到的 386 指令）；`FE 38 nn` = 呼叫 Rust 服務
  └─ mem.rs      1 MB 記憶體 + I/O 埠：8259 PIC、8253 PIT、8042 鍵盤、CMOS、VGA、OPL2
  └─ vga.rs      四個平面、寫入模式 0–3、鎖存器、讀取模式 1、屬性控制器、DAC、CRTC 幾何
  └─ opl.rs      YM3812：埠端（位址、計時器、偵測）+ 合成器（在音訊執行緒）
  └─ machine.rs  執行迴圈、虛擬時鐘、IRQ 遞送、BIOS：INT 08/09/10/11/12/15/16/1A/33
  └─ dos.rs      DOS 5.0：MCB 記憶體鏈、PSP、EXEC/常駐/結束、檔案控制代碼（JFT→SFT）、找檔
  └─ saveui.rs   存檔畫面擴充（記憶體修補 MAIN.EXE，見下）
  └─ host.rs     minifb 原生視窗與輸入；audio.rs cpal 輸出
```

每個中斷向量一開始都指向 F000 段裡的小樁：`FE 38 nn`（呼叫 Rust）+ `RETF 2`。
程式掛向量、再串回舊向量時，最後都會落到同一個樁，所以常駐程式的掛鉤行為和真機一樣。

遊戲資料夾（有 `REKO3.COM` 的那一層）就是 C: 磁碟的根目錄。`assets/` 是標準 VGA BIOS 的 8×8 / 8×16 字型
（MAIN 直接讀 F000:FA6E 的 ROM 字型畫 ASCII）。

## 存檔畫面擴充（src/saveui.rs）

MAIN.EXE 載入時在記憶體裡修補（硬碟上的檔案不動；每個原始位元組都對上才會修補，所以別的版本只會維持原樣）：

- 存檔／讀檔清單從 5 列改成 10 列（清單、旗標、指標表搬進加大的堆疊框架）。
- 戰場功能選單加「存檔」「讀檔」（選單表在框架裡重建，字串也要在 DS 裡，選單常式忽略遠指標的段）。
- 每次存檔另存整台機器狀態 `savestates/slotN.state` + `slotN.key`（當時的 MSAVE 檔頭）；讀檔時檔頭相符就還原狀態。
- 第 10 格：每次我方階段正常結束（2687:2f56 裡 3682 返回、戰場結束旗標為 0）自動存。
- 各畫面關窗後只重畫場景區，10 列清單會蓋到外框，所以清單開啟前存整張可見畫面、關閉時貼回。

關鍵位址、坑與欄位格式都寫在 `src/saveui.rs` 開頭的註解。

## 網頁版（web/）

同一份模擬器編成 `wasm32-wasip1` 的 reactor（`web/src/lib.rs`，對外函式 `eik_start` / `eik_run` / `eik_frame` / `eik_mouse` / `eik_key` / `eik_audio` / `eik_fs_writes`）。
`web/www/` 是網頁：

- `wasi.js`：自己實作的 WASI preview1（只有 wasm 用到的那些呼叫）加記憶體檔案系統；遊戲資料夾掛在 `/game`。
  WASI 不能複製檔案控制代碼，所以 `Dos` 的快照是用檔名重新開檔（原生版也走同一條路）。
- `engine.js`：載入 wasm、包裝成 `run(ms)` / `frame()` / `mouse()` / `key()` / `audio()`。
- `app.js`：IndexedDB 保存遊戲檔與存檔（看 `eik_fs_writes` 有沒有變）、canvas、觸控／滑鼠／鍵盤、Web Audio。
- `zip.js`：讀 zip（瀏覽器內建 `DecompressionStream`）、寫 zip（匯出存檔）。

```
rustup target add wasm32-wasip1
sh web/build.sh                                   # 產生 web/www/eiketsu_web.wasm
python -m http.server 8000 --directory web/www    # 用瀏覽器開 http://localhost:8000
cargo test --release -p eiketsu-web               # web 介面（原生跑）
node web/www/test.mjs <遊戲資料夾> web/www/eiketsu_web.wasm harness/fixtures/sishui_hall   # 真正的 wasm + wasi.js
```

push 到 `main` 時，`.github/workflows/pages.yml` 會自動編譯並部署到 GitHub Pages。

## 命令列

`eiketsu-rt [--game DIR] [--run PROGRAM] [--headless] [--steps N] [--trace] [--shot FILE.png] [--script FILE] [--replay JOURNAL] [--no-journal] [--mute] [--state 快照]`

## 操作日誌（重現問題）

時間由執行的指令數決定（每毫秒 25,000 條指令），時鐘與日期從固定的起點開始（1993/08/01 09:00），所以相同的輸入一定得到相同的結果。
視窗模式會把所有輸入（滑鼠、按鍵）連同發生時的指令數寫進遊戲資料夾的 `journal/session-<時間>.txt`。
讀取「有整機狀態的存檔」後，日誌會接到新的 `…-loadN.txt`，旁邊附上當時讀的狀態檔。

`eiketsu-rt --headless --replay 日誌檔 --steps N --shot x.png` 可以精確重播到任何一刻（讀過狀態的日誌另加 `--state`）。

## 驗收（harness）

```
cargo test --release                              # 單元 + BIOS + 端到端（需要遊戲檔放在上一層）
EIKETSU_CASE=02 cargo test --release --test e2e   # 只跑某個情境
```

端到端測試在**遊戲資料夾的暫存複本**上無頭執行（不會動到真正的存檔），在指定的模擬毫秒點擊、按鍵，
在檢查點比對畫面雜湊，並直接從記憶體與寫出的檔案核對結果。截圖存在 `harness/out/`。黃金值都是先人工看過截圖才寫入的。
驗收用的存檔放在 `harness/fixtures/`（不跟著自己的遊戲進度變）。

| 情境 | 驗證內容 |
|---|---|
| 01_password_title | 開機 → 密碼畫面（TFDED 中文字、BIOS 8×8 字型）→ 輸入 → 標題選單 |
| 02_new_game_prologue | 開始新遊戲 → 序章標題 → 董卓插圖與劇情對白 |
| （e2e.rs）save_screens… | 讀第 2 格（夾具：汜水關戰前）→ 存到第 7 格（10 列存檔清單）→ 回合結束自動存第 10 格（核對檔頭位元組）→ 遊戲結束不覆蓋 → 讀第 10 格回到戰場、敵軍走法相同 |
| （e2e.rs）battle_menu… | 大廳存第 7 格 → 戰場功能選單存第 4 格 → 回合結束 → 讀第 4 格回到存的那一刻 → 讀第 1 格（原附存檔）出現提示、戰場不變 → 讀第 7 格回到大廳 |
| （e2e.rs）journal_replay | 一段遊玩的日誌重播後，畫面與暫存器必須完全相同 |
| （e2e.rs）state_reads_the_battlefield | 從記憶體讀出的戰場部隊與遊戲畫面一致 |
| （bios.rs）int1c_hook_may_clobber_ds | INT 1Ch 掛鉤改 DS 不能影響被中斷的程式 |

劇本語法：`<毫秒> click X Y | rclick X Y | move X Y | key 名稱 | shot 檔案 | expect 名稱 雜湊 | expectfile 寫出檔 參考檔 | quit`
（滑鼠用驅動程式座標：x 0–639、y 為螢幕列。跟 `--state 快照` 一起用時，毫秒從快照的時間點往後算。）

## 除錯工具

- `--trace`：印出所有 DOS/BIOS 呼叫與檔案開啟（含呼叫端堆疊）。`EIKETSU_TRACE_PORTS=1`：印出所有 I/O 埠存取。
- 出錯時會印出最後 64 條指令的 CS:IP（`[trail]`）。
- `EIKETSU_BREAK=段:位移,…`：執行到這些位址時印出暫存器（找遊戲程式碼用）。
- `EIKETSU_WATCH=線性位址,…`：印出改寫這些位元組的指令（只抓 CPU 寫入，DOS 讀檔寫入不算）。
- `EIKETSU_DUMP=STEP,N`：在第 STEP 步寫出 `dump.bin`（記憶體 + VGA 平面），以及之後 N 條指令的暫存器到 `trace.txt`，
  可以拿 unicorn 從同一快照逐條比對，判斷是不是 CPU 指令實作有錯。

## 遠端控制與戰場狀態

`--control PORT` 開一個本機 TCP 控制埠（`--window X,Y,W,H` 指定無邊框視窗位置、`--speed N` 加速）。
`eiketsu-rt ctl PORT 指令`：`click X Y`、`rclick`、`move`、`key 名稱`、`type 文字`、`wait 毫秒`、`shot 檔案`、`speed N`、`status`、
`state`（直接從記憶體讀出戰場所有部隊：陣營、名字、地圖座標、兵力、等級、經驗、士氣、道具）。
控制埠送進去的輸入一樣寫進日誌，可以重播；受控視窗不吃實體滑鼠鍵盤。

戰場畫面：每格 32 像素，地圖區左上 (16,72)。格子 (x,y) 的畫面中心 = (32×(x−捲動x)+32, 32×(y−捲動y)+88)。
記憶體配置（差分快照找到的，詳見 src/battle.rs）：武將表每筆 0x2C 位元組（+0x19 兵力、+0x1C 等級、+0x1D 經驗、+0x1E 起道具），
部隊表在武將表 +0x685D、每筆 14 位元組（+1 x、+2 y、+8 陣營、+0xA 士氣）。

## 已知限制

- OPL2 合成器是浮點模型，不是逐位元精確；音色接近但不等於實機。音訊只在視窗模式開啟（`--mute` 關閉）。
- 文字模式畫面不繪製（只有啟動器的錯誤訊息會用到，內容會印到終端機）。
- 沒有 Sound Blaster 數位音效（SBOPL2 只用 OPL2）、沒有 EMS/XMS（遊戲不需要）。
- macOS / Linux 尚未實機測試；用到的 minifb、cpal 都支援。
