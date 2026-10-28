# p4os: build, flash và chạy

p4os là kernel viết bằng Rust, chạy bare-metal trên ESP32-P4 **rev v1.x**. Board mục tiêu là **Guition JC4880P443C**: màn 4,3" 480×800 ST7701S, cảm ứng GT911, codec ES8311, chip Wi-Fi ESP32-C6. Nó không dùng esp-hal, ESP-IDF hay FreeRTOS.

## Yêu cầu

- Rust stable kèm target `riscv32imafc-unknown-none-elf` (khai báo trong `rust-toolchain.toml`).
- `esptool` ≥ 5 và `pyserial`.
- Board cắm qua cổng USB-Serial-JTAG, mặc định là `/dev/ttyACM0` (đổi bằng biến `PORT`).

## Lệnh

| Việc | Lệnh |
|---|---|
| Build ELF và đóng image app | `scripts/build-image.sh` → `out/p4os.bin` |
| Build rồi flash vào phân vùng `factory` (0x10000) | `scripts/flash.sh` |
| Reset board và xem console | `python3 scripts/monitor.py [giây]` |
| Giữ quyền dùng board riêng khi nhiều tiến trình chạy song song | `scripts/board-lock.sh <lệnh>` |
| Reset, gửi lệnh shell rồi in phản hồi | `python3 scripts/shell-session.py ps mem demo` |

Thư mục build của cargo là `out/` (cấu hình trong `.cargo/config.toml`).

## Cách khởi động

1. ROM nạp bootloader cấp 2 ESP-IDF v5.5 có sẵn ở 0x2000. Phần này tạm thời vẫn giữ.
2. Bootloader ánh xạ hai segment flash: rodata cùng app descriptor ở `0x40000020`, code ở `0x41000040`. Sau đó nó nhảy vào `_start` (`kernel/src/boot.rs`).
3. Kernel làm các bước sau:
   - tắt các watchdog;
   - nâng CPU từ 90 MHz lên 360 MHz;
   - bật CLIC và tick 1 kHz;
   - khởi động scheduler với hai thread `shell` và `idle`.

Bố cục bộ nhớ nằm trong `kernel/link.x`. Bootloader bắt buộc có đúng hai segment flash, và segment 0 phải bắt đầu bằng `esp_app_desc_t`.

## Kiểm tra màn hình bằng camera

Có một camera Imou nhìn thẳng vào màn hình, dùng được qua `scripts/cam.py`. Mật khẩu RTSP lấy từ `~/.config/p4os/imou` (quyền 600). Địa chỉ camera mặc định là `192.168.88.7`, đổi được bằng `P4OS_CAM`.

| Việc | Lệnh |
|---|---|
| Tìm khung màn hình (nháy xám/đen), lưu vào `scripts/cam-roi.json` | `python3 scripts/cam.py calibrate` |
| Tô một màu và xác nhận camera thấy đúng màu đó | `python3 scripts/cam.py check 400000` |
| Chụp vùng màn hình, đứng chiều, tự giảm đèn nền xuống 3% khi chụp | `python3 scripts/cam.py shot out.png` |
| Chạm giả lập rồi chụp | `python3 scripts/board-ui.py "tap 80 132" "shot calculator"` |
| Chỉnh ảnh camera qua ONVIF (độ sáng, tương phản) | `python3 scripts/cam_onvif.py get` / `set Brightness=35` |
| Kiểm tra loa: phát âm thử và nghe bằng micro của camera | `python3 scripts/cam_audio.py 1000` |
| Kết nối Wi-Fi bằng thông tin lưu trong `~/.config/p4os/wifi` (mật khẩu được che trong mọi output) | `python3 scripts/wifi-connect.py` |

Camera tự cân sáng nên bị cháy sáng khi panel hiện màu sáng. Vì vậy hãy dùng màu tối (khoảng 0x40) khi kiểm tra.

Mở cổng serial mà **không đổi DTR/RTS**. Nếu chỉ đổi một trong hai đường, USB-Serial-JTAG sẽ reset chip.

## Shell debug

Các lệnh: `help`, `uptime`, `clock`, `irq`, `ps`, `mem`, `demo`, `membench`, `lcd` (trạng thái luồng hình), `hwscan` (dò phần cứng), `vpg on|off` (vạch màu thử của DSI host), `fill RRGGBB|off` (tô toàn màn hình một màu), `bl 0..100` (độ sáng đèn nền), `tap X Y` (giả lập một lần chạm), `sd` (dò thẻ SD), `ls [/sd/...]` (liệt kê thư mục), `audio` (trạng thái âm thanh), `tone [Hz]` (phát âm thử 3 giây), `c6` (dò chip Wi-Fi), `wifi connect SSID PASS`, `get http://...` (tải một URL), `apps` (liệt kê app user trong flash), `run SLOT [ARG]` (chạy một app user). Nhấn **Ctrl-T** bất cứ lúc nào để in bảng thread ngay từ ngữ cảnh ngắt, kể cả khi mọi thread đã treo.

## App user (U-mode, cách ly bằng PMP)

App user là binary riêng, không link vào kernel. Kernel nạp app từ flash vào một vùng RAM riêng rồi chạy nó ở U-mode. App chỉ đọc/ghi được vùng nhớ của chính nó và gọi kernel qua `ecall` (syscall).

| Việc | Lệnh |
|---|---|
| Build `userapps/` và đóng gói thành image | `scripts/build-userapps.sh` → `out/userapps/hello.p4app`, `crash.p4app` |
| Ghi image vào slot N (0..13, địa chỉ 0x720000 + N×0x10000). Script tự giữ khoá board | `scripts/flash-app.sh 0 out/userapps/hello.p4app` |
| Liệt kê các slot hợp lệ (tên, kích thước) | shell: `apps` |
| Chạy app ở slot N với tham số tuỳ chọn. Shell chờ đến khi app kết thúc | shell: `run 0`, `run 1 3` |

- Nếu app truy cập ngoài vùng của nó, kernel chỉ dừng app đó và in một dòng `*** app 'crash' (thread 4) killed: store access fault ...`. Kernel, màn hình và shell vẫn chạy bình thường. `run 1 N` (N = 0..6) thử từng kiểu vi phạm, xem bảng trong `userapps/crash/src/main.rs`.
- Giới hạn đã biết: bootloader ESP-IDF khoá PMP entry 0–2, nên app vẫn chạm được CLIC, cache controller và ROM.
- ABI syscall, định dạng image và cơ chế PMP: [plans/261028-1217-user-mode-apps/design.md](../plans/261028-1217-user-mode-apps/design.md).
- Khi build trong git worktree, cargo gộp cả `.cargo/config.toml` của repo cha nên `-Tlink.x` bị lặp. Khi đó hãy build kernel với `RUSTFLAGS="-C link-arg=-Tlink.x"`.

## Khôi phục firmware gốc

```
esptool --chip esp32p4 --port /dev/ttyACM0 write-flash 0 ~/backups/esp32p4/flash-factory-30eda0e19d99.bin
```

## Những điều riêng của P4 v1.x đã kiểm chứng

- CLIC ở đây khác chuẩn. Ngưỡng ngắt nằm ở thanh ghi `0x20800008` (bit 31:24), không phải CSR. Ngoại vi dùng các đường CLIC 16..47.
- Khi `mret` sang một thread khác, `mcause` phải giữ bit ngắt và mã nguyên nhân của trap đang xử lý. Nếu không, đường ngắt đó kẹt ở trạng thái "đang phục vụ" và không bao giờ kích hoạt lại (xem `kernel/src/trap.rs`).
- Thứ tự đổi bộ chia khi tăng xung là APB → SYS → MEM → CPU, sau đó mới chọn nguồn CPLL. Lý do: MEM_CLK phải ≤ 200 MHz và APB_CLK ≤ 100 MHz.

## Màn hình: những điều đã kiểm chứng

- Panel là ST7701S 480×800, dùng 2 lane ở 500 Mbps, xung pixel 30 MHz (PLL_F240M/8). Timing H 12/42/42, V 2/8/166. Bảng lệnh khởi tạo nằm trong `kernel/src/display.rs`.
- **Reset LCD ở GPIO5** (mức thấp là reset). Thiếu bước reset này thì panel vẫn ACK lệnh nhưng màn hình chỉ đen. Đèn nền nối vào chân EN của mạch boost MP3202 qua GPIO23, mức cao là bật.
- Thông tin chung về board EV của Espressif (EK79007, 1024×600, đèn nền GPIO26) **không đúng** với board này. Nguồn đáng tin là gói tài liệu JC4880P443 của nhà sản xuất cùng các thanh ghi đọc qua JTAG từ firmware gốc (xem `scripts/regdump.sh`).

## Ngoại vi khác

| Thiết bị | Trạng thái | Ghi chú |
|---|---|---|
| Thẻ SD (SDMMC slot 0, 4-bit, 20 MHz) | Đọc và ghi block chạy được. Filesystem FAT32 có tên file dài, mount ở `/sd` | Phải bật LDO4 3,3 V trước. CPU đọc trực tiếp FIFO (offset 0x200) **không lấy được dữ liệu ra khỏi FIFO** trên chip này, nên driver dùng IDMAC với bộ đệm trung gian ở SRAM không qua cache. |
| Âm thanh (I2S1 → ES8311 → loa, PA ở GPIO11) | DMA, I2S và codec chạy, đã kiểm chứng bằng thanh ghi | Chưa gắn loa nên chưa nghe thử được. |
| Đèn nền | PWM 20 kHz qua LEDC trên GPIO23 | Thanh độ sáng trong Settings điều khiển trực tiếp. |
| Cảm ứng GT911 (I2C 0x5D) | Driver và cách ánh xạ toạ độ đã có | Chưa kiểm chứng bằng chạm thật. |
| Wi-Fi (ESP32-C6 qua ESP-Hosted v2, SDIO) | Chạy: kết nối WPA2, DHCP, DNS, TCP, HTTP/1.0 (smoltcp). App Browser tải được trang `http://` trên board | Lúc nhận dạng thẻ phải giữ D3 (GPIO17) ở mức cao như GPIO thường. Host báo sẵn sàng bằng cách ghi `0x01` vào SCRATCH_7 (0x08C). Mỗi lần CMD53 phải đọc **đúng** số byte đang chờ, đọc dư sẽ làm C6 ngừng trả lời. Firmware slave v2.3.x cần gửi đủ `threshold` và `pmf_cfg` trong cấu hình STA, nếu thiếu C6 sẽ treo. Chưa hỗ trợ HTTPS (chưa có TLS) và chưa có BLE. |

Một bẫy với systimer: comparator chỉ kích hoạt khi bộ đếm **bằng** giá trị đích. Nếu deadline đã trôi qua lúc ngắt đang tắt, tick sẽ dừng vĩnh viễn. `time::arm_after` xử lý trường hợp này bằng cách bỏ qua các chu kỳ bị lỡ và cộng bù số tick.
