Use CodexBridge for project `test`.

Before doing any project work, initialize/join that project with `chatgpt_turn_init` and follow the brief and project instructions it returns. On later turns, follow the CodexBridge turn protocol and use the previous turn reference automatically.

Task: Bạn đang kiểm thử một CodexBridge đang chạy thực tế. Mục tiêu là kiểm tra behavior của public tool contract, không audit source code và không sửa production source. Hãy thực hiện các kiểm thử bên dưới bằng chính các public tools mà CodexBridge cung cấp, tự xác minh kết quả sau mỗi mutation, và báo cáo PASS/FAIL theo từng case.

## Nguyên tắc bắt buộc

1. Ở đầu user turn này, gọi `chatgpt_turn_init` đúng một lần trước các project-scoped tools khác.
2. Chỉ tạo dữ liệu thử trong thư mục project-relative `codexbridge-test-scratch/`. Không sửa source production, Git history, remote, release hay deployment.
3. Với mọi `apply_patch`, KHÔNG được tin riêng response `success/applied`. Sau mỗi patch phải dùng `read_file` hoặc `exec_command` đọc lại nội dung/bytes thực tế của mọi file bị tác động. Đây là yêu cầu quan trọng để phát hiện partial-apply/tool-harness bugs.
4. Không né lỗi. Nếu behavior khác expected, ghi FAIL cùng response/error code và trạng thái file thực tế.
5. Không thêm test code vào repository. Đây là black-box/end-to-end test bằng public tools.
6. Nếu một case phụ thuộc platform Unix/Linux mà runtime không hỗ trợ, ghi SKIP với lý do cụ thể; không tính là PASS.
7. Cuối cùng cleanup toàn bộ `codexbridge-test-scratch/`, xóa memory keys dùng cho test bằng `remember(key, value="")`, và clear plan bằng `update_plan(plan=[])`.

## A. Tool discovery và filesystem cơ bản

### A1. Directory/list/tree/glob
- Tạo các file:
  - `codexbridge-test-scratch/a.txt` = `alpha\n`
  - `codexbridge-test-scratch/sub/b.txt` = `beta\n`
  - `codexbridge-test-scratch/sub/c.log` = `gamma\n`
- Dùng `list_directory` với page nhỏ để kiểm tra `offset/max_results`, `truncated`, `next_offset` nếu schema hỗ trợ.
- Dùng `tree` trên scratch với depth nhỏ.
- Dùng `glob` cho `**/*.txt` và xác minh chỉ nhận `a.txt`, `sub/b.txt`.
- PASS khi kết quả sorted/bounded và continuation đúng contract.

### A2. `read_file` long-line continuation
- Dùng `exec_command` tạo `codexbridge-test-scratch/long.txt` chứa một dòng UTF-8 dài khoảng 300-400 KB, có marker `BEGIN-🙂-` ở đầu và `-END` ở cuối.
- Đọc bằng `read_file` với budget nhỏ để buộc `truncated=true` và `next_line_byte_offset`.
- Tiếp tục bằng CHÍNH `next_offset` + `next_line_byte_offset` được trả về cho tới hết.
- Ghép phần content lại và xác minh marker đầu/cuối không mất byte, UTF-8 không hỏng.

### A3. FIFO/non-regular rejection (Unix/Linux)
- Dùng `exec_command` tạo FIFO `codexbridge-test-scratch/test.fifo` bằng `mkfifo`, KHÔNG mở writer.
- Gọi `read_file` vào FIFO.
- PASS nếu trả lỗi nhanh kiểu `INVALID_INPUT`/non-regular-file; FAIL nếu call treo/chờ peer hoặc đọc FIFO như file thường.

### A4. Symlink no-follow
- Tạo symlink `codexbridge-test-scratch/outside-link` trỏ tới `/etc/hosts`.
- `read_file` phải reject, không trả nội dung `/etc/hosts`.
- `grep` trong scratch không được follow symlink để lộ nội dung target.

## B. `grep` và bounded search

### B1. Search correctness
- Search `alpha|beta|gamma` trong scratch, có context và pagination nhỏ.
- Xác minh path/line number đúng.
- Nếu `traversal_limit_hit=true`, không được coi result là exhaustive.

### B2. Binary/invalid UTF-8 handling
- Tạo file có bytes invalid UTF-8 trong scratch.
- `grep` không được crash; phải skip/bounded-error theo contract.

## C. `apply_patch` — ưu tiên cao

### C1. Multi-file atomic success với target khác nhau
- Tạo `patch-one.txt = one\n`, `patch-two.txt = two\n`.
- Trong MỘT `apply_patch`, update cả hai path khác nhau.
- Sau response, bắt buộc `read_file` lại cả hai file.
- PASS khi cả hai cùng thay đổi đúng.

### C2. First update chunk không cần explicit `@@`
- Tạo `no-at-at.txt = old\n`.
- Apply:

```text
*** Begin Patch
*** Update File: codexbridge-test-scratch/no-at-at.txt
-old
+new
*** End Patch
```

- PASS nếu update thành `new\n`.

### C3. Context identity + line-ending preservation
- Dùng Python/exec tạo file bytes chính xác: `delete-me\r\n  keep-me  \nlast\r\n`.
- Patch xóa `delete-me`, giữ context `  keep-me  ` và thay `last` thành `LAST`.
- Sau patch dùng Python đọc `repr(open(...,'rb').read())`.
- PASS nếu context giữ nguyên whitespace và line ending gốc, không bị normalize ngoài vùng cần thay đổi.

### C4. Move/add destination không overwrite
- Chuẩn bị source và destination đều tồn tại.
- Thử move vào destination đã tồn tại.
- PASS nếu reject và cả source + destination đều giữ nguyên.

### C5. Delete directory safety
- Tạo `codexbridge-test-scratch/delete-dir/nested/keep.txt`.
- Gọi một patch duy nhất:

```text
*** Begin Patch
*** Delete File: codexbridge-test-scratch/delete-dir
*** End Patch
```

- PASS nếu tool trả error và directory/nested file vẫn tồn tại nguyên vẹn.

### C6. CRITICAL — duplicate same-path action / Codex1 partial-apply regression

Đây là case bắt buộc. Nó nhằm phát hiện bug đã từng thấy ở một `apply_patch` harness: trong một patch có hai `*** Update File` riêng cùng path, tool đã có lúc chỉ áp một action nhưng vẫn báo patch applied.

1. Tạo file chính xác:

```text
FIRST
SECOND
```

ở `codexbridge-test-scratch/duplicate-target.txt`.

2. Trong CHÍNH MỘT `apply_patch` call, gửi:

```text
*** Begin Patch
*** Update File: codexbridge-test-scratch/duplicate-target.txt
@@
-FIRST
+FIRST-CHANGED
*** Update File: codexbridge-test-scratch/duplicate-target.txt
@@
-SECOND
+SECOND-CHANGED
*** End Patch
```

3. Expected behavior của verified CodexBridge/Codex-style tool path: REJECT TOÀN BỘ vì nhiều operations target cùng resolved path (`INVALID_PATCH`, `duplicate patch target`, hoặc `multiple operations target ...`).

4. NGAY SAU tool call, bất kể response là success hay error, gọi `read_file` lại file.

5. PASS duy nhất khi file vẫn EXACTLY:

```text
FIRST
SECOND
```

6. FAIL CRITICAL nếu xảy ra bất kỳ trường hợp nào:
- chỉ `FIRST` đổi;
- chỉ `SECOND` đổi;
- cả hai đổi dù tool contract đáng ra reject;
- tool báo success nhưng file ở trạng thái partial/mixed;
- tool báo error nhưng file vẫn bị mutate.

Trong báo cáo cuối, ghi riêng case này là `C6 Codex1 duplicate-target regression` và nêu cả response lẫn file content sau call.

### C7. Transaction rollback
- Trong một patch gồm ít nhất hai target khác nhau, cho operation đầu hợp lệ và operation sau chắc chắn conflict/missing-context.
- PASS nếu operation đầu cũng không được commit, tức all-or-rollback.

## D. `view_image`

### D1. Valid image
- Tạo một PNG 1x1 hợp lệ trong scratch (có thể dùng Python stdlib/base64 fixture).
- `view_image` phải trả image content block/mime hợp lệ.

### D2. Signature-only corrupt PNG
- Tạo file bắt đầu bằng PNG signature nhưng body hỏng.
- `view_image` phải reject vì decode thất bại; magic bytes đơn thuần không đủ.

## E. Process execution / `exec_command` + `write_stdin`

### E1. Basic execution + network/YOLO/Podman
- Chạy `pwd`, xác minh working directory project-relative.
- Chạy `podman info`.
- Chạy container thật, ví dụ:
  `podman run --rm docker.io/library/alpine:3.22 sh -c 'echo podman-in-podman-ok'`
- Nếu `podman run` rootless fail vì namespace/mount/permission của environment nhưng project cho phép rootful Podman, retry bằng **explicit** `sudo podman run ...`; không dựa vào shell alias. Ghi lại cả failure đầu và fallback thành công.
- PASS khi một container thật chạy thành công. Nếu default Bubblewrap không usable hoặc Podman không usable trong Bubblewrap, native YOLO fallback được chấp nhận; native fallback không tự cấp thêm quyền cho rootless Podman. Không coi network availability là lỗi; project này chủ đích có network.

### E2. Long-running session continuation
- Start command in > initial yield window để nhận `session_id`.
- Poll bằng `write_stdin` trên cùng session, KHÔNG restart command.
- PASS khi final status/output thu được từ cùng session.

### E3. Finished-but-truncated recovery
- Chạy command tạo output đủ lớn để bounded output buffer hoặc `max_output_tokens` truncate, nhưng process kết thúc nhanh.
- PASS nếu response finished+truncated vẫn giữ `session_id` và có thể gọi `write_stdin(since_output_offset=...)` để replay retained final output.

### E4. Head+tail cursor truthfulness
- Tạo output gồm marker `HEAD-UNIQUE`, một middle section rất lớn, rồi `TAIL-UNIQUE`.
- Sau eviction, thử replay từ cursor nằm trong vùng middle đã bị evict.
- PASS nếu response có omission marker và tiếp tục từ retained tail; không được replay stale head bytes như thể chúng là contiguous bytes tại cursor đó.

### E5. stderr logical-line prefix
- Chạy Python ghi stderr thành nhiều raw writes cho cùng một logical line, ví dụ write `ERR`, flush/sleep, rồi `OR\n`.
- PASS nếu marker/prefix stderr không bị chèn vào giữa thành dạng `ERR[prefix]OR`.

### E6. PTY external/native signal
- Với `tty=true`, chạy shell tự `kill -TERM $$`.
- PASS nếu final result có `completion_reason=signaled`, native signal tương ứng (Unix SIGTERM=15), và không giả thành normal exit.

### E7. Requested signal bị trap rồi process tự exit
- Start PTY command kiểu:
  `sh -c 'trap "exit 42" TERM; while :; do sleep 1; done'`
- Dùng `write_stdin` gửi terminate + wait for exit.
- Expected: `requested_signal=terminate`, nhưng native wait status là normal exit 42, nên `completion_reason=exited`, `exit_code=42`, `signal=null`.
- FAIL nếu CodexBridge suy `signaled` chỉ vì đã từng request TERM.

## F. Persistent state / continuity

### F1. `remember` + direct `recall`
- Lưu ít nhất 3 keys có prefix `codexbridge-e2e-`.
- Recall trực tiếp từng key và xác minh exact value.

### F2. Stable pagination + stale snapshot detection
- Recall page với `max_results=1`, lấy `next_offset` và `snapshot_hash`.
- Sau page đầu, chèn một memory key mới làm thay đổi sorted enumeration.
- Continue bằng old `snapshot_hash`.
- PASS nếu nhận `PAGINATION_STALE`, không silently duplicate/skip.
- Sau đó restart từ `offset=0` không dùng old hash và enumeration phải hoạt động.

### F3. Persistent plan lifecycle
- `update_plan` với 2-3 steps, tối đa một `in_progress`.
- `recall(include_plan=true)` phải thấy plan.
- `update_plan(plan=[])` phải clear.
- Recall lại phải thấy plan null/absent.

## G. Instructions / nested AGENTS scope

### G1. Nested instruction disclosure before mutation
- Trong scratch tạo một nested `AGENTS.md` có rule rõ ràng, ví dụ yêu cầu file `.txt` dưới scope phải chứa marker `NESTED-RULE-COMPLIED`.
- Sau đó thử `apply_patch` hoặc `exec_command` lần đầu vào path sâu hơn thuộc scope đó.
- Nếu CodexBridge trả `AGENTS_SCOPE_REQUIRED`, đọc/consume disclosure rồi retry với mutation tuân thủ rule.
- PASS nếu mutation đầu không xảy ra trước disclosure và retry tuân thủ rules mới thành công.

### G2. Instruction precedence semantics
- Xác minh project AGENTS/rules chỉ override repository-working guidance, không được coi là có quyền override higher-priority system/user instructions, turn synchronization, project identity hoặc factual tool/security contract.

## H. Skills

### H1. `skills_list`
- Gọi catalogue cho project/root thích hợp.
- Không coi warning malformed skill là lý do bỏ qua valid skills.

### H2. `skills_read`
- Nếu catalogue có skill phù hợp, chọn một skill, đọc toàn bộ `SKILL.md` bằng continuation tới `truncated=false`.
- Nếu limit chia giữa UTF-8 char, tool phải progress hoặc explicit error; không được trả `truncated=true` với unchanged cursor.
- Nếu project không có skill nào, ghi SKIP H2, không FAIL.

## I. Cleanup và báo cáo

1. Xóa toàn bộ `codexbridge-test-scratch/` sau khi đã hoàn tất mọi read-back verification.
2. Xóa mọi memory key `codexbridge-e2e-*` bằng `remember(..., value="")`.
3. Clear plan bằng `update_plan(plan=[])`.
4. Xác minh scratch đã biến mất và plan/memory test data không còn.

Báo cáo cuối theo bảng:

| Case | PASS/FAIL/SKIP | Evidence ngắn | Error/code nếu có |
|---|---|---|---|

Sau bảng, có 3 mục ngắn:
- `Critical findings`
- `Non-critical differences`
- `Cleanup status`

Nếu C6 có partial mutation, phải đặt nó đầu tiên trong `Critical findings` và ghi rõ rằng đây là regression tương tự bug `Codex1 apply_patch duplicate same-path partial-apply`.
Về skills, nếu không có file thì bạn tự tạo ra để test.
