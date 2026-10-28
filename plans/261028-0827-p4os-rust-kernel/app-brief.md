# Brief cho agent viết app p4os

Bạn được giao viết một hoặc vài app có sẵn cho p4os, một OS Rust chạy trên ESP32-P4 với màn hình cảm ứng 1024×600. App chạy trên desktop có thanh trạng thái 36 px và thanh tiêu đề 56 px. Vùng app được truyền vào `update`, rộng 1024 và cao khoảng 508 px; khi bàn phím ảo đang mở thì vùng này thấp hơn.

## Đọc trước khi viết

- `sdk/src/lib.rs`: trait `App`, `AppInfo`, `System` (fs, audio, net, stats, decode_image, brightness, run_command, launch, now).
- `gui/src/ui.rs`: các widget immediate-mode (`button`, `button_colored`, `icon_button`, `toggle`, `slider`, `progress`, `text_field`, `scroll`, `list`, `text`, `panel`, `interact`), cùng `Input`, `Key`, `Theme`.
- `gui/src/canvas.rs`: các hàm vẽ (`fill_rect`, `fill_round_rect`, `stroke_round_rect`, `fill_circle`, `stroke_circle`, `line`, `fill_triangle`, `blit`, `blit_scaled`, `fill_gradient`, clip).
- `gui/src/font.rs`: `Font::measure`, `draw`, `wrap`. Các font nằm trong `ui.fonts.{small,body,large,title,bold,huge,mono}`; `huge` chỉ có ký tự ASCII.
- `apps/src/calculator.rs`: app mẫu, dùng làm khuôn cho cấu trúc, icon và cách đọc phím.
- `apps/src/desktop.rs`: cách desktop gọi app.

## Quy tắc

- Code là `no_std` + `alloc`, không được dùng `std`. Hàm toán số thực lấy từ `libm`; crate `apps` đã có sẵn dependency này.
- **Chỉ sửa file app của bạn** (liệt kê trong prompt) và thư mục dữ liệu thử của bạn trong `sim/sdcard/`. Không sửa `gui/`, `sdk/`, `desktop.rs`, `lib.rs` hay bất kỳ `Cargo.toml` nào. Nếu cần đổi API hoặc thêm dependency, ghi rõ trong báo cáo cuối và tìm cách khác trong lúc chờ.
- Không dùng dữ liệu giả. App phải đọc dữ liệu thật qua `System`. Trên board thật, file đọc từ thẻ SD qua `/sd/...`; trong simulator, `/sd` trỏ vào `sim/sdcard`.
- `update` không được block. Việc nặng như mạng hay âm thanh đã có thread của OS lo, app chỉ cần poll.
- Giữ nguyên `INFO.id` đã có. Icon tự vẽ bằng primitives, có thể dùng `crate::icons::tile`.
- Giao diện phải ổn khi chạm bằng ngón tay (nút cao ≥ 48 px) và theo `ui.theme`.
- Code rõ ràng, comment ở mức vừa phải giống phần code đang có. Viết unit test (`#[cfg(test)]`) cho phần logic không dính giao diện.

## Build và kiểm tra

Bạn làm việc trong một **bản sao workspace riêng** (đường dẫn nằm trong prompt). Mọi lệnh chạy trong thư mục đó, sau khi `. ~/.cargo/env`.

- Build cho chip: `cargo build -p p4os-apps` (target mặc định là riscv). Bắt buộc phải sạch lỗi và sạch warning ở file của bạn.
- Test logic: `cargo test -p p4os-apps --target aarch64-unknown-linux-gnu`.
- Chụp ảnh giao diện qua simulator:
  `cargo sim -- "launch <id>; tap X Y; type abc; key enter; wait 500; shot out/shots/<id>-1.png"`
  - Các lệnh có sẵn: `tap`, `drag X1 Y1 X2 Y2`, `type`, `key enter|backspace|esc|up|down|left|right`, `wait`, `launch`, `shot`.
- Xem ảnh vừa chụp bằng công cụ Read để tự kiểm tra trực quan. Lặp lại đến khi giao diện gọn gàng, không có chữ tràn hay đè lên nhau.

## Kết quả nộp

- File app hoàn chỉnh.
- 2–3 ảnh chụp đẹp nhất mỗi app, đặt trong `out/shots/` của bản sao.
- Báo cáo ngắn: tính năng đã làm, giới hạn, và đề nghị thay đổi API nếu có. Kết thúc bằng dòng `Status: DONE | DONE_WITH_CONCERNS | BLOCKED`.

Múi giờ Europe/London. Viết báo cáo bằng tiếng Anh.
