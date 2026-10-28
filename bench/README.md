# Benchmark p4os và ESP-IDF/FreeRTOS

Thư mục này chứa cùng một bộ micro-benchmark chạy trên hai hệ điều hành, đo trên cùng một board:

- p4os: lệnh shell `bench`, mã nguồn ở `kernel/src/bench.rs`.
- ESP-IDF v5.5 / FreeRTOS: app trong `freertos/`.

Bộ test đo các mục sau:

- chuyển ngữ cảnh;
- yield;
- mutex;
- độ trễ ngắt và độ trễ từ ISR đến lúc thread chạy lại;
- heap. Riêng p4os có thêm các hàng `heap frag N holes`: thời gian alloc/free khi heap có N lỗ trống. Bản FreeRTOS không có các hàng này. Hàng memcpy SRAM của p4os dùng 2 × 127 KiB thay vì 2 × 128 KiB, vì vùng SRAM 256 KiB phải chứa thêm header của khối;
- memcpy;
- CRC32 và một kernel số nguyên. Hai phép đo này không phụ thuộc OS.

Kết quả, phương pháp đo và các lưu ý nằm trong [`plans/reports/benchmark-261028-p4os-vs-freertos.md`](../plans/reports/benchmark-261028-p4os-vs-freertos.md). Log gốc nằm trong `results/`.

| File | Vai trò |
|---|---|
| `freertos/` | Project ESP-IDF. Build bằng `idf.py build`. `sdkconfig.defaults` khóa các thông số: rev < v3, 360 MHz, PSRAM 200 MHz, unicore, tick 1 kHz, `-O2`. `sdkconfig.lean` là bản tắt thêm các tính năng debug, dùng để kiểm tra độ nhạy. |
| `run-on-board.sh LABEL=IMAGE...` | Nạp từng image IDF vào 0x10000 và lưu log. Sau đó nạp lại p4os và chạy `bench`. Phải chạy bên trong `scripts/board-lock.sh`. |
| `capture.py` | Reset board, có thể gửi một lệnh shell, rồi đọc console cho tới khi gặp dòng `bench: done`. |

Script chỉ ghi vào phân vùng app tại 0x10000. Bootloader, bảng phân vùng và phân vùng storage được giữ nguyên.
