# Benchmark: p4os và ESP-IDF/FreeRTOS trên cùng một board ESP32-P4

Ngày đo: 2026-10-28. Board: Guition JC4880P443C, ESP32-P4 rev v1.0, CPU 360 MHz (đo được 359 MHz), PSRAM 32 MiB ở 200 MHz, L2 cache 256 KiB.

## Kết luận nhanh

- **Các primitive của kernel trên p4os nhanh hơn rõ rệt.** Chuyển ngữ cảnh qua semaphore ping-pong nhanh hơn 2,5 lần, `yield` nhanh hơn 1,8 lần, mutex nhanh hơn 1,4 lần. Từ lúc ngắt xảy ra đến lệnh đầu tiên của handler cũng nhanh hơn 1,5 đến 1,7 lần.
- **Bộ cấp phát heap của p4os nhanh hơn khoảng 7 đến 9 lần**, nhưng chỉ trong điều kiện bench này, khi heap gần như chưa phân mảnh. Allocator first-fit của p4os có độ phức tạp O(n) theo số khối trống, còn TLSF của IDF là O(1).
- **FreeRTOS thắng ở ba chỗ:**
  - Đánh thức thread từ ISR nhanh hơn khoảng 10% (dùng task notification thì nhanh hơn khoảng 17%).
  - Độ trễ ngắt **tối đa** thấp hơn nhiều: 0,8 µs so với khoảng 3 µs của p4os. Phần lớn chênh lệch này là do p4os đang chạy GUI, màn hình và audio trong lúc đo (xem mục Lưu ý).
  - `memcpy` nhanh hơn 1,56 lần trên SRAM và 1,12 lần trên PSRAM, nhờ dùng newlib thay vì `compiler_builtins` của Rust.
- **Phần tính toán thuần CPU gần như ngang nhau.** CRC32 bằng nhau tuyệt đối. Kernel số nguyên do GCC biên dịch nhanh hơn LLVM khoảng 7%. Như vậy chênh lệch ở các primitive đến từ thiết kế OS, không phải từ chip hay compiler.
- Tắt các mặc định thiên về debug của IDF (bản "lean") chỉ cải thiện FreeRTOS khoảng 5 đến 20%. Khoảng cách với p4os phần lớn đến từ cấu trúc của port FreeRTOS trên P4.

## Bảng kết quả

Mỗi ô là **median của 5 lần chạy** trong phiên `261028-133053`. Phiên thứ hai (`261028-133136`) cho median lệch không quá 3,5% ở mọi dòng trung bình. Các dòng "max" biến động nhiều hơn. Log gốc nằm trong `bench/results/`.

| Phép đo | Đơn vị | p4os | FreeRTOS (IDF mặc định) | FreeRTOS lean | FreeRTOS ÷ p4os |
|---|---|---:|---:|---:|---:|
| Chuyển ngữ cảnh (semaphore ping-pong), mỗi lần switch | ns | **2263.8** | 5612.4 | 5142.5 | 2.48× |
| Yield giữa 2 thread cùng priority | ns | **1283.5** | 2316.0 | 1997.9 | 1.80× |
| Mutex lock+unlock, không tranh chấp | ns | **698.2** | 963.7 | 919.3 | 1.38× |
| Độ trễ ngắt TB, thread đang block (deadline → handler) | ns | **542.7** | 812.3 | 687.4 | 1.50× |
| Độ trễ ngắt **max**, thread đang block | ns | 3250.0 | **812.5** | 687.5 | 0.25× |
| Độ trễ ngắt TB, CPU đang bận (deadline → handler) | ns | **555.5** | 802.6 | 687.4 | 1.44× |
| Vào ngắt TB, CPU đang bận (lệnh bị ngắt → handler, đo bằng mcycle) | ns | **330.1** | 570.0 | 454.5 | 1.73× |
| ISR → thread chạy lại, TB (semaphore) | ns | 2544.5 | **2319.3** | 2162.2 | 0.91× |
| ISR → thread chạy lại, max (semaphore) | ns | 9011.1 | **5289.7** | 4980.5 | 0.59× |
| ISR → thread chạy lại, TB (task notification) | ns | không có | 2123.5 | 1948.5 | |
| ISR → thread chạy lại, max (task notification) | ns | không có | 5192.2 | 4752.1 | |
| Heap alloc+free 32 B (SRAM) | ns | **369.0** | 2973.3 | 2736.8 | 8.06× |
| Heap alloc+free 1 KiB (SRAM) | ns | **457.9** | 3199.9 | 2934.7 | 6.99× |
| Heap alloc+free 256 KiB (PSRAM) | ns | **363.0** | 3261.7 | 2977.5 | 8.99× |
| memcpy SRAM→SRAM (128 KiB × 8) | MB/s | 237.7 | **371.8** | 377.2 | 0.64× |
| memcpy PSRAM→PSRAM (1 MiB) | MB/s | 47.8 | **53.6** | 53.8 | 0.89× |
| CRC32 1 MiB (64 KiB SRAM × 16) | MB/s | 32.5 | 32.5 | 32.5 | 1.00× |
| Kernel số nguyên, mỗi vòng lặp | ns | 80.3 | **74.8** | 74.8 | 0.93× |

Cột cuối là tỉ lệ thời gian FreeRTOS chia cho p4os, tính trên bản IDF mặc định. Với các dòng MB/s, tỉ lệ được đảo lại để quy ước vẫn thống nhất: lớn hơn 1 nghĩa là p4os nhanh hơn. Ở 360 MHz, 1 ns ≈ 0,36 chu kỳ. Ví dụ, một lần switch của p4os tốn khoảng 815 chu kỳ, của FreeRTOS khoảng 2020 chu kỳ.

Checksum giống hệt nhau trên cả ba bản build: `crc32=0x1db8cdd8`, `int=0x6ac225a8`. Hai giá trị này cũng khớp với kết quả tính độc lập bằng Python (`zlib.crc32` và cùng công thức), nên hai bản cài đặt Rust và C đang làm cùng một khối lượng việc.

## Phương pháp

Hai chương trình chạy cùng bộ test với cùng tham số và in bảng theo cùng một định dạng:

- p4os: lệnh shell `bench`, mã nguồn ở `kernel/src/bench.rs`.
- FreeRTOS: app ESP-IDF ở `bench/freertos/main/bench_main.c`.

Mỗi test chạy 5 lần. Bảng in ra median, min và max của 5 lần đó.

**Đồng hồ đo.** Thời gian được đo bằng `mcycle` (độ phân giải 1 chu kỳ CPU). Tần số CPU được đo lúc chạy bằng cách so `mcycle` với systimer trong 10 ms.

**Độ trễ ngắt** được đo bằng comparator 1 của systimer, nối với bộ đếm unit 0:
- p4os dùng comparator 0 cho tick.
- IDF dùng comparator 0 cho tick và comparator 2 cho esp_timer. Comparator 1 chỉ dành cho tick của core 1, nên ở chế độ unicore nó đang rảnh.

Ngắt được đặt ở level 3 trên cả hai hệ:
- p4os: `irq::attach`, CLIC line 20.
- IDF: `esp_intr_alloc(..., ESP_INTR_FLAG_LEVEL3 | ESP_INTR_FLAG_IRAM)`.

Handler đọc `mcycle` trước, sau đó đọc systimer bằng đúng một chuỗi thanh ghi trên cả hai hệ: ghi UPDATE, chờ VALID, đọc HI rồi LO.

### Cách đo từng phép

| Phép đo | Cách đo |
|---|---|
| Ping-pong | Hai thread cùng priority (cao nhất trong hệ, trừ task esp_timer của IDF vốn đứng yên). A: `give(PING); take(PONG)`. B làm ngược lại. Chạy 10 000 vòng. Kết quả = thời gian ÷ (2 × 10 000). Con số này gồm cả give/take và việc block, không phải chi phí switch thuần. |
| Yield | Hai thread cùng priority, mỗi thread gọi yield 10 000 lần (p4os dùng `sched::yield_now`, FreeRTOS dùng `taskYIELD`). Kết quả = thời gian ÷ 20 000. |
| Mutex | 100 000 lần lock, tăng biến đếm, unlock. p4os: `sync::Mutex`. FreeRTOS: `xSemaphoreCreateMutex`, `Take`/`Give`. |
| IRQ, thread đang block | Đặt alarm cách hiện tại 50 µs, rồi thread chờ trên semaphore mà ISR sẽ release. Lặp 1000 mẫu. "Độ trễ ngắt" = giá trị systimer đọc trong handler trừ deadline (độ phân giải 62,5 ns). "ISR → thread" = `mcycle` lúc thread chạy lại trừ `mcycle` ở đầu handler. Bản FreeRTOS đo thêm `vTaskNotifyGiveFromISR`/`ulTaskNotifyTake`. |
| IRQ, CPU đang bận | Thread vừa quay vòng vừa ghi `mcycle` vào một biến. Handler lấy `mcycle` của chính nó trừ giá trị cuối cùng trong biến, tức là chi phí từ lệnh bị ngắt đến thân handler. Sai số không quá một vòng lặp (vài chu kỳ). |
| Heap | 10 000 cặp alloc+free. p4os dùng global allocator: khối ≥ 64 KiB tự đi sang PSRAM. IDF dùng `heap_caps_malloc` với `MALLOC_CAP_INTERNAL\|8BIT` hoặc `MALLOC_CAP_SPIRAM`. Trước khi đo, chương trình kiểm tra địa chỉ trả về có nằm đúng vùng nhớ mong muốn không. |
| memcpy | SRAM chỉ có 768 KiB nên phép đo SRAM chép một buffer 128 KiB 8 lần (tổng 1 MiB). Phép đo PSRAM chép 1 MiB sang 1 MiB. Không gọi writeback cache, và cả hai hệ đều làm như vậy. |
| CRC32 | CRC-32 IEEE dạng bảng 256 phần tử, chạy 16 lần trên một buffer SRAM 64 KiB điền bằng xorshift32. |
| Kernel số nguyên | 1 000 000 vòng gồm xorshift, nhân, xoay bit, chia và một nhánh phụ thuộc dữ liệu. Số vòng và seed đi qua `black_box`/`volatile` để compiler không gập thành hằng số. |

### Cấu hình hai bên

| | p4os | ESP-IDF/FreeRTOS |
|---|---|---|
| Phiên bản | commit gốc 7e865fb cộng thêm `bench` | ESP-IDF v5.5.6 (`~/esp/esp-idf-v5.5.6`) |
| Compiler | rustc stable / LLVM, `opt-level=3`, LTO fat, `codegen-units=1` | riscv32-esp-elf GCC, `-O2` (`CONFIG_COMPILER_OPTIMIZATION_PERF`) |
| Chip | rev v1.x, bootloader IDF v5.5 có sẵn trên board | `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y`, min rev v1.0, max v1.99 |
| CPU / PSRAM / L2 | 360 MHz / 200 MHz / 256 KiB | 360 MHz / 200 MHz / 256 KiB (`CONFIG_CACHE_L2_CACHE_256KB`) |
| Số core | 1 (core 1 nằm yên trong ROM) | `CONFIG_FREERTOS_UNICORE=y`, mọi task pin vào core 0 |
| Tick | 1 kHz | `CONFIG_FREERTOS_HZ=1000` |
| Watchdog | tắt hết | Task WDT và Interrupt WDT đều tắt |
| Code chạy từ | flash qua cache (XIP) | kernel, ISR, heap, memcpy nằm trong IRAM (L2MEM, cũng đi qua cache L1); code của app nằm trên flash |
| Tác vụ nền | GUI, luồng màn hình (DMA mỗi frame, ngắt level 6), audio (level 5), shell | không có, chỉ có idle và esp_timer |

Bản "FreeRTOS lean" (`bench/freertos/sdkconfig.lean`) tắt thêm assertion (`CONFIG_COMPILER_OPTIMIZATION_ASSERTIONS_DISABLE`), hardware stack guard và kiểm tra tràn stack. Bản này chỉ để kiểm tra độ nhạy, không phải cấu hình IDF mà người ta hay dùng.

Chỉ ghi vào phân vùng app tại 0x10000. Bootloader ở 0x2000, bảng phân vùng và phân vùng storage ở 0x710000 không bị đụng tới. Bảng phân vùng đã được đọc lại từ board, và `bench/freertos/partitions.csv` sao y bố cục đó. Sau mỗi phiên, p4os được nạp lại.

## Lưu ý về tính công bằng và cách đọc số

1. **Tải nền không giống nhau, và điều này bất lợi cho p4os.**
   - p4os đo trong lúc GUI vẫn vẽ, DMA màn hình vẫn bắn ngắt level 6 mỗi frame và thread audio vẫn chạy. Các thread này dùng `irq::free` (che toàn bộ ngắt), kể cả bên trong allocator.
   - Các dòng **max** của p4os (độ trễ ngắt khoảng 3 µs, đánh thức tối đa 7 đến 9 µs) phản ánh critical section dài nhất của các thread nền cộng với các ngắt priority cao hơn. Đó không phải chi phí của đường đi ngắt.
   - App FreeRTOS gần như không có việc gì khác để làm.
   - Các dòng **trung bình** ít bị ảnh hưởng, vì min và max của 5 lần chạy rất sát nhau.
2. **Tính năng không tương đương.**
   - Mutex của FreeRTOS có kế thừa priority, kiểm tra chủ sở hữu và timeout. `Mutex` của p4os chỉ là semaphore nhị phân, không có kế thừa priority.
   - Port FreeRTOS cho P4 phải quản lý thêm trạng thái FPU, PIE (lệnh AI/SIMD) và hardware loop, đồng thời lập trình lại stack guard ở mỗi lần switch. p4os chỉ lưu FPU khi bị "dirty" và không hỗ trợ PIE hay HWLP.
   - Yield ở cấp task của IDF đi qua một ngắt mềm (`esp_crosscore_int_send_yield`) rồi chờ ngắt đó xảy ra. Còn đường switch từ ISR chỉ là bật cờ để switch lúc thoát ngắt. Vì vậy với FreeRTOS, "ISR → thread" rẻ hơn hẳn so với ping-pong ở cấp task.
3. **Heap.**
   - IDF dùng TLSF (O(1)), cộng thêm bước chọn heap theo capability, khóa và danh sách nhiều vùng heap. Tổng cộng khoảng 1000 chu kỳ cho mỗi cặp alloc+free.
   - p4os dùng first-fit với danh sách khối trống sắp theo địa chỉ. Lúc đo, danh sách gần như rỗng nên rất nhanh. Khi heap bị phân mảnh, thời gian của p4os sẽ tăng tuyến tính theo số khối trống, còn của IDF thì không.
   - Ngoài ra, p4os che ngắt trong suốt thời gian duyệt danh sách. Đây là một nguồn gây ra độ trễ ngắt tối đa ở mục 1.
4. **Khác biệt về compiler và thư viện.**
   - CRC32 bằng nhau tuyệt đối (khoảng 11 chu kỳ mỗi byte). Ở vòng lặp này, giới hạn nằm ở phụ thuộc dữ liệu và tra bảng, không nằm ở chất lượng code sinh ra.
   - Ở kernel số nguyên, GCC `-O2` nhanh hơn LLVM `-O3` khoảng 7% (27 so với 29 chu kỳ mỗi vòng).
   - `memcpy` của IDF là bản newlib nằm trong IRAM. Rust dùng `memcpy` của `compiler_builtins`, chạy chậm hơn 36% trên SRAM. Đây là chỗ p4os có thể cải thiện mà không cần đổi kernel.
5. **Ý nghĩa và giới hạn của các con số độ trễ ngắt.**
   - "deadline → handler" gồm: độ trễ phần cứng từ lúc comparator khớp đến lúc trap, đường vào trap của OS, và thời gian đọc systimer trong handler.
   - "Vào ngắt (mcycle)" loại được phần phần cứng và phần đọc systimer.
   - Hiệu của hai dòng này gần như hằng số trên cả hai hệ: 210 đến 240 ns. Điều đó cho thấy hai phép đo nhất quán với nhau.
   - Giá trị systimer có độ phân giải 62,5 ns. Vì vậy các dòng max bị lượng tử hóa, còn dòng trung bình (1000 mẫu) thì không.
6. **Vị trí của code.** Toàn bộ p4os chạy từ flash qua cache. Các đường nóng của IDF nằm trong IRAM. Trong vòng lặp bench, code đều đã nằm sẵn trong cache L1 nên ảnh hưởng nhỏ. Với code "nguội" (lần ngắt đầu tiên, các đường hiếm đi qua), p4os có thể chịu thêm cache miss từ flash.
7. **Phạm vi đo hẹp.** Kết quả chỉ đến từ một board, một chip rev v1.0, hai phiên × 5 lần chạy, cùng ngày, cùng nhiệt độ phòng. Benchmark này cũng không đo throughput dưới tải thật như mạng, SD hay đồ họa.
8. "kernel tick worst alarm latency since boot" (15 µs) mà p4os in ra tính cả giai đoạn khởi động, khi driver che ngắt khá lâu. Không nên so sánh con số này với các dòng trên.

## Tái lập kết quả

```bash
# ESP-IDF (một lần): clone v5.5.6 vào ~/esp, rồi chạy ./install.sh esp32p4
# và idf_tools.py install cmake (bản aarch64 không có sẵn cmake).
. ~/esp/esp-idf-v5.5.6/export.sh
cd bench/freertos
idf.py -B ~/esp/p4bench-artifacts build
idf.py -B ~/esp/p4bench-lean-artifacts -D SDKCONFIG=$HOME/esp/p4bench-lean-artifacts/sdkconfig \
       -D SDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.lean" build
cd ../..
scripts/board-lock.sh bench/run-on-board.sh \
  freertos=$HOME/esp/p4bench-artifacts/p4bench.bin \
  freertos-lean=$HOME/esp/p4bench-lean-artifacts/p4bench.bin
```

`bench/run-on-board.sh` chỉ ghi vào 0x10000. Nó chạy từng image IDF và lưu log vào `bench/results/<nhãn>-<thời điểm>.log`. Sau đó nó nạp lại p4os, gửi lệnh `bench` và lưu `p4os-<thời điểm>.log`. Nếu một bước bị lỗi, p4os vẫn được nạp lại.

Trên board đang chạy p4os, có thể gõ trực tiếp `bench` trong shell.

## Câu hỏi còn mở

- Có nên tách riêng thêm một phép đo p4os trong lúc GUI và màn hình tạm dừng, để dòng max so sánh được với FreeRTOS hay không? Hiện p4os chưa có lệnh nào dừng luồng màn hình.
- Hai chỗ p4os có thể làm nhanh hơn: `Semaphore::release` luôn duyệt hết 32 bit waiter kể cả khi không có ai chờ, và `memcpy` hiện dùng bản của `compiler_builtins`. Hai việc này nằm ngoài phạm vi benchmark.
