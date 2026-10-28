# p4os: hệ điều hành viết bằng Rust cho ESP32-P4 rev v1.x

Trạng thái (2026-10-28): phase 1–6 xong và đã kiểm chứng trên board, gồm cả màn hình qua camera. Âm thanh chạy về mặt điện tử nhưng chưa gắn loa. Wi-Fi và HTTP qua ESP32-C6 đã chạy. Còn lại: 3b (nhân thứ hai), 7–8 (app chạy U-mode, nạp app động), TLS, BLE. Board thật là Guition JC4880P443C.

## Mục tiêu

Một OS mở, viết từ đầu bằng Rust, chỉ nhắm ESP32-P4 rev v1.x (board ESP32-P4-Function-EV, chip v1.0). Người khác viết được ứng dụng Rust chạy trên OS này. Đây là dự án học và nghiên cứu, ưu tiên hiểu phần cứng và hiệu năng.

## Ràng buộc

- Runtime không dùng esp-hal, ESP-IDF hay FreeRTOS. Mã ESP-IDF v5.5 (`~/ref/esp-idf`, bản sparse) chỉ được dùng làm tài liệu thanh ghi, vì header v5.5 viết cho silicon v1.x.
- Tạm giữ bootloader cấp 2 ESP-IDF v5.5 có sẵn trên flash ở 0x2000 và bảng phân vùng gốc. Kernel nạp vào phân vùng `factory` ở 0x10000. Viết bootloader Rust riêng là việc của phase sau.
- Backup flash gốc: `~/backups/esp32p4/flash-factory-30eda0e19d99.bin` (có SHA256SUMS).

## Ngoài phạm vi

- Không có MMU phân trang, tiến trình chỉ được cách ly bằng PMP.
- Không hỗ trợ chip P4 v3.x.

## Các phase

| # | Phase | Tiêu chí hoàn thành |
|---|---|---|
| 1 | Boot và console (xong) | Kernel Rust khởi động từ bootloader, tắt watchdog, in ra USB-Serial-JTAG, trap handler in mcause/mepc, đo được tần số CPU so với systimer |
| 2 | Ngắt và thời gian (xong) | CLIC + interrupt matrix, ngắt systimer, `sleep` chính xác, đọc nhập từ console |
| 3 | Lập lịch (xong, 1 nhân) | Thread preemptive có stack riêng, ưu tiên, mutex/semaphore. Còn lại (3b): nhân HP thứ hai, stack ngắt riêng, thu hồi thread đã thoát |
| 4 | Bộ nhớ (xong) | Heap trong SRAM, khởi tạo PSRAM 32 MB, allocator nhiều vùng |
| 5 | Màn hình (xong: ST7701 480×800) | LDO, MIPI DSI, panel EK79007 1024×600, framebuffer trong PSRAM, đo FPS |
| 6 | Nhập liệu và lưu trữ (xong: SD + FAT32; cảm ứng chưa thử chạm thật) | I2C + GT911, SDMMC + FAT, đọc file trên thẻ |
| 7 | Syscall và SDK | App chạy U-mode, cách ly bằng PMP, ABI syscall ổn định, crate `p4os-sdk` |
| 8 | Nạp app động | App ELF trên thẻ SD được kernel nạp và chạy |
| 9 | Âm thanh (driver xong), tăng tốc đồ hoạ (chưa làm), mạng (xong: Wi-Fi + TCP/IP + HTTP) | ES8311/I2S, PPA/DMA2D, ESP-Hosted tới chip Wi-Fi phụ |

## Cấu trúc mã

- `kernel/`: crate binary `no_std`, gồm boot, trap, driver, scheduler.
- `scripts/`: đóng image, flash, monitor (dùng esptool và pyserial có sẵn trên Pi).

## Rủi ro chính

- Cấu hình clock, PSRAM và CLIC trên v1.x không có tài liệu Rust nào. Phải đối chiếu header C và TRM.
- Flash sai không làm hỏng board, vì có thể nạp lại bản backup bằng `esptool write-flash 0 <backup>`.
