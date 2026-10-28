# Viết OS bằng Rust cho ESP32-P4: nghiên cứu và đề xuất

Ngày: 2026-10-28

## Kết luận

Viết OS 100% bằng Rust cho ESP32-P4 là làm được. Nhưng con chip đang cắm trên Pi là **rev v1.0**, và esp-hal (HAL Rust chính thức của Espressif) **không hỗ trợ chip dưới v3.0**, cũng không nhận đóng góp để thêm vào. Muốn thuần Rust trên board này thì phải tự viết toàn bộ lớp driver.

Có ba hướng đi:

1. **Thuần Rust, hiệu năng cao nhất (khuyến nghị):** mua board P4 dùng chip v3.x, rồi dựng OS trên esp-hal + Embassy.
2. **Giữ board hiện tại:** dùng ESP-IDF làm nền, viết toàn bộ phần OS phía trên bằng Rust qua `esp-idf-svc`.
3. **Thuần Rust trên chip v1.0:** tự viết HAL. Hợp để học sâu, nhưng tốn nhiều tháng công và không ai bảo trì cùng bạn.

## Bằng chứng

| Đo/tra được | Nguồn |
|---|---|
| Chip trên board: ESP32-P4 rev v1.0, ROM eco2, chạy 360 MHz, flash 16 MB, PSRAM 32 MB | esptool + log khởi động đọc trực tiếp trên board |
| esp-hal đánh dấu hỗ trợ P4 hoàn tất từ 15/06/2026, nhưng chỉ cho v3.x | esp-hal issue #2285 |
| Nhóm esp-rs, 09/03/2026: chip v1.x "ở mức silicon là con chip rất khác", không hỗ trợ, khuyên dùng ESP-IDF | esp-hal issue #2285 |
| Roadmap: "v1.3 chip support is not planned and contributions to add it will not be accepted" | esp-hal issue #3962 |
| esp-hal 1.2 có driver MIPI DSI (DBI/DPI) cho P4, nằm sau feature `unstable` | docs.espressif.com, esp_hal::mipi_dsi |
| Còn thiếu hoặc đang làm trên P4: PARLIO, LP core, CSI, ISP, PPA, CAM | esp-hal issues #2285, #6409 |
| Target Rust `riscv32imafc-esp-espidf` hỗ trợ P4 (`std`, IDF ≥ 5.2) | doc.rust-lang.org, platform-support/esp-idf |

## Vì sao chip v1.0 khó làm thuần Rust

- **Bộ ngắt CLIC** của v1.x là phiên bản khác với v3.x và với C5/C61. Lõi ngắt của esp-hal không dùng lại được.
- **PSRAM 200 MHz** cần khởi tạo MSPI và dò pha DQS. Nếu làm sai, toàn bộ framebuffer và heap trong PSRAM sẽ bị hỏng ngẫu nhiên.
- **Wi-Fi/BLE** không nằm trên P4. Chúng đi qua giao thức ESP-Hosted (SDIO) tới chip phụ. Muốn có mạng thì phải tự viết phía host của giao thức này bằng Rust.
- **Màn hình** cần MIPI DSI PHY, bộ khởi tạo panel EK79007 và cấu hình LDO. Cảm ứng cần GT911 qua I2C, thẻ nhớ cần SDMMC + FAT, âm thanh cần ES8311 qua I2S. Mỗi thứ là một driver riêng phải viết.
- v1.0 chạy tối đa 360 MHz, còn v3.x lên được 400 MHz.

## Kiến trúc đề xuất (áp dụng cho hướng 1, phần lớn cũng dùng được cho hướng 3)

P4 có PMP nhưng không có MMU phân trang, nên "OS" ở đây là một hệ điều hành nhúng kiểu RTOS. Nó không thể là Linux thu nhỏ với tiến trình có không gian địa chỉ riêng.

- **Nhân và lập lịch:** dùng executor async của Embassy, mỗi nhân một executor (P4 có 2 nhân HP). Executor ưu tiên cao dành cho tác vụ thời gian thực như âm thanh và cảm ứng. Không nên tự viết scheduler nếu mục tiêu là hiệu năng: Embassy gần như không tốn chi phí chuyển ngữ cảnh và không cần stack riêng cho từng tác vụ.
- **Cách ly ứng dụng (tuỳ chọn):** dùng PMP theo kiểu Tock nếu muốn chạy app không tin cậy. Cái giá là thêm độ trễ syscall.
- **Đồ hoạ (đây là chỗ quyết định hiệu năng):** framebuffer 1024×600 RGB565 nặng khoảng 1,2 MB/khung và nằm trong PSRAM. Nên dùng double buffer kết hợp với DMA2D và PPA của P4 để blend/xoay bằng phần cứng, và chỉ vẽ lại vùng thay đổi. Phần UI dùng `embedded-graphics` hoặc Slint (Slint có backend MCU). Driver PPA hiện chưa có trong esp-hal nên phải tự viết.
- **Bộ nhớ:** đặt code nóng và stack trong SRAM/TCM, đặt heap lớn và framebuffer trong PSRAM. Cấp phát dùng `esp-alloc`, chia nhiều vùng.
- **Lưu trữ:** thẻ SD dùng `embedded-sdmmc` (FAT), flash nội dùng `sequential-storage` hoặc `ekv`.
- **Mạng:** cần lớp ESP-Hosted host viết bằng Rust, rồi `embassy-net`. Bản thân esp-radio không chạy Wi-Fi trực tiếp trên P4 vì P4 không có radio.

## So sánh ba hướng

| | Thuần Rust trên chip v3 | Rust trên ESP-IDF (board hiện tại) | Thuần Rust trên chip v1.0 |
|---|---|---|---|
| Đúng yêu cầu "Rust hết" | Có | Không: nhân và driver vẫn là C | Có |
| Hiệu năng | Cao nhất, 400 MHz, không có lớp FreeRTOS | Tốt; PPA/JPEG có sẵn driver IDF | Cao nhưng giới hạn 360 MHz |
| Thời gian ra màn hình đầu tiên | Vài ngày | Vài giờ | Vài tuần đến vài tháng |
| Rủi ro | Phải tự viết driver PPA, ESP-Hosted, ES8311 | Thấp | Rất cao: tự lo CLIC, PSRAM, clock, DSI |
| Chi phí | Mua board mới | 0 | 0 |

## Bước tiếp theo nếu chọn hướng 1

1. Mua board P4 có chip v3.x. Khi mua cần kiểm tra lại revision bằng `esptool chip-id`.
2. Cài `espup` và `probe-rs` trên Pi. Board có USB-JTAG nên debug được trực tiếp.
3. Mốc đầu tiên: Embassy trên 2 nhân, PSRAM, DSI hiện một framebuffer, GT911 đọc được cảm ứng. Đo FPS toàn màn hình làm baseline.
4. Sau đó tới PPA/DMA2D, thẻ SD, âm thanh, và cuối cùng là ESP-Hosted cho mạng.

## Câu hỏi còn mở

- Bạn muốn "OS" để làm gì: sản phẩm, học kiến trúc OS, hay launcher chạy nhiều app như demo Brookesia? Câu trả lời quyết định có cần cách ly bằng PMP và có cần tải app động không.
- Bạn có sẵn sàng mua board chip v3.x không?
- Có cần Wi-Fi/BLE ngay giai đoạn đầu không? Đây là driver tốn công nhất.
