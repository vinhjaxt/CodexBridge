Use CodexBridge for project `test`.

Before doing any project work, initialize/join that project with `chatgpt_turn_init` and follow the brief and project instructions it returns. On later turns, follow the CodexBridge turn protocol and use the previous turn reference automatically.

Task: Bạn đang thực hiện một **black-box / E2E / real-world stress audit** cho CodexBridge bằng **chính các public tools mà CodexBridge expose cho bạn**.

Mục tiêu không phải chỉ chứng minh happy-path hoạt động, mà phải chủ động tìm:

* correctness bugs;
* silent data corruption;
* partial mutations;
* pagination bugs;
* stale cursor/snapshot bugs;
* output-loss/recovery bugs;
* behavior không khớp schema/description;
* vấn đề khi project rất lớn;
* resource-boundary bugs;
* unexpected interaction giữa các tools;
* instruction-scope bugs;
* long-running process/session lifecycle bugs;
* symlink/FIFO/special-file safety bugs;
* UTF-8/binary edge cases;
* concurrency/session isolation issues;
* cleanup/resource leaks;
* behavior khiến một coding agent thực tế có thể đưa ra quyết định sai.

## Quy tắc bắt buộc

Chỉ dùng **public tools của CodexBridge** và các command được chạy thông qua `exec_command`.

Không đọc source implementation của CodexBridge để suy ra expected behavior. Đây là black-box test.

Không sửa:

* production source của CodexBridge;
* Git history;
* branch;
* commit;
* remote;
* release;
* deployment;
* CI;
* daemon configuration.

Mọi fixture phải nằm trong một scratch directory riêng, ví dụ:

```text
codexbridge-realworld-e2e/
```

Không được làm ảnh hưởng file có sẵn ngoài scratch.

Memory fixture phải dùng key prefix riêng:

```text
codexbridge-rw-e2e-
```

Process/container fixture phải dùng prefix riêng:

```text
tmp-cbrw-
```

Cuối test phải cleanup hoàn toàn các fixture này.

Không coi một test PASS chỉ vì tool không crash. Phải kiểm tra **semantic correctness bằng read-back hoặc observable evidence**.

Nếu tool trả:

```text
truncated=true
incomplete=true
next_offset
next_line_byte_offset
snapshot_hash
session_id
output_offset
output_next_offset
continuation
```

thì phải thực sự exercise continuation contract tương ứng.

Nếu schema public không cho truyền một field mà response yêu cầu caller truyền, đánh FAIL và xác định rõ:

```text
runtime/server bug
canonical MCP schema bug
connector/model-facing schema stale
hay environment limitation
```

Không được gộp các lớp này lại.

---

# Phase 0 — Discovery / contract sanity

Trước khi tạo fixture:

1. Liệt kê public tools hiện có.
2. Ghi lại schema public của ít nhất:

   * `read_file`
   * `exec_command`
   * `write_stdin`
   * `recall`
   * `apply_patch`
3. Kiểm tra các continuation-related inputs có được expose hay không:

   * `read_file.line_byte_offset`
   * `write_stdin.since_output_offset`
   * `write_stdin.wait_for_exit_ms`
   * `recall.snapshot_hash`
   * `exec_command.extensions`
   * `write_stdin.extensions`
   * `recall.extensions`
4. Nếu initialize metadata có `serverInfo.version`, kiểm tra version có contract suffix dạng tương đương:

```text
<version>+contract.<hash>
```

Không dừng test nếu schema thiếu field. Ghi finding rồi tiếp tục những test vẫn thực hiện được.

---

# Phase 1 — Large-project fixture

Tạo scratch project đủ lớn để kiểm tra behavior thực tế nhưng không cố DoS máy.

Mục tiêu:

* khoảng **10,000–15,000 files**;
* ít nhất **100 nested directories**;
* tổng dung lượng khoảng **30–80 MiB**;
* nhiều filename gần giống nhau;
* repeated symbols/text;
* `.gitignore`;
* nested `AGENTS.md`;
* text files;
* source-like files;
* binary/invalid UTF-8;
* mixed LF/CRLF;
* one very long logical line;
* files có Unicode filename/content.

Ví dụ structure:

```text
codexbridge-realworld-e2e/
  AGENTS.md
  src/
    module-000/
    ...
    module-099/
  services/
    api/
      AGENTS.md
    worker/
  generated/
  ignored/
  binary/
  huge/
```

Dùng shell generation để tạo fixture hiệu quả. Không gọi `apply_patch` hàng nghìn lần.

Nested `AGENTS.md` phải chứa một rule dễ kiểm chứng, ví dụ yêu cầu file tạo dưới scope đó phải chứa marker:

```text
NESTED-RULE-COMPLIED
```

Tạo ít nhất:

* một file > 5 MiB;
* một logical line khoảng 1–2 MiB;
* một file mixed LF/CRLF;
* một invalid UTF-8 file;
* một symlink tới file ngoài scratch nếu platform cho phép;
* một FIFO nếu platform hỗ trợ.

---

# Phase 2 — Large-project navigation

## L1 — bounded directory listing

Dùng `list_directory` với `max_results` rất nhỏ trên directory lớn.

Xác minh:

* result bị bounded;
* sorted ổn định;
* `truncated=true` khi cần;
* `next_offset` tiến lên;
* pagination tới EOF;
* không duplicate;
* không skip.

Thu thập toàn bộ tên qua các page nhỏ và so với shell-generated expected list.

## L2 — glob trên project lớn

Tạo marker chỉ xuất hiện ở một số path xác định.

Chạy glob như:

```text
**/*.txt
src/**/*.rs
**/target-name-*.txt
```

Kiểm:

* deterministic order;
* pagination;
* ignore rules;
* không leak ignored files;
* không lặp path.

## L3 — grep/search high-cardinality

Tạo:

* marker xuất hiện 1 lần;
* marker xuất hiện 100 lần;
* marker xuất hiện hàng nghìn lần;
* same marker ở nhiều nested directories.

Kiểm:

* pagination;
* line numbers;
* path;
* deterministic continuation;
* `traversal_limit_hit`;
* `incomplete`;
* `skipped_files`.

Không được gọi kết quả exhaustive nếu server báo `incomplete=true`.

## L4 — search + mutation race

Thực hiện một search/pagination nhiều page.

Giữa hai page, thay đổi một fixture file bằng public tool.

Kiểm xem continuation:

* có duplicate;
* skip;
* stale result;
* explicit restart/error;
* hay behavior nào không được mô tả.

Không kết luận bug nếu API rõ ràng document search snapshot không stable; nhưng phải ghi operational consequence.

---

# Phase 3 — Filesystem edge cases

## F1 — huge logical line

Dùng `read_file` trên long-line fixture với giới hạn nhỏ.

Phải đi hết file bằng:

```text
offset
next_offset
next_line_byte_offset
line_byte_offset
```

Xác minh:

* cursor luôn tiến;
* không infinite loop;
* UTF-8 không bị split;
* byte đầu/cuối đúng;
* marker giữa/cuối không mất.

## F2 — exact EOF boundaries

Test:

* offset đúng EOF;
* offset sau EOF;
* zero-length page nếu API cho phép;
* limit quá nhỏ để chứa một UTF-8 character.

Expected:

* không stuck cursor;
* explicit error nếu page không thể progress.

## F3 — FIFO

Nếu platform hỗ trợ:

```text
mkfifo
```

`read_file` phải reject nhanh và **không block chờ writer**.

## F4 — symlink

Test symlink:

```text
scratch-link -> /etc/hosts
```

hoặc một file ngoài scratch/project.

Kiểm:

* direct read;
* grep;
* copy;
* move;
* patch nếu relevant.

Không được follow target ngoài allowed boundary.

## F5 — invalid UTF-8 / binary

Search invalid UTF-8 fixture.

Tool:

* không crash;
* không trả garbage như text hợp lệ;
* phải indicate incomplete/skipped nếu đó là contract.

---

# Phase 4 — Real coding workflow trong project lớn

Giả lập một task coding thực tế:

> Tìm tất cả nơi liên quan đến marker `RW_FEATURE_FLAG`, hiểu call/config relationship bằng available tools, rồi thay đổi behavior ở 3 files thuộc 2 directories khác nhau, trong đó một file nằm dưới nested AGENTS scope.

Không dùng shell `sed` trực tiếp để sửa; dùng public mutation tool phù hợp.

Kiểm:

1. discovery có tìm đủ target cần thiết không;
2. nested AGENTS rule có được disclose trước mutation không;
3. first mutation dưới undisclosed nested scope có bị chặn trước khi file thay đổi không;
4. retry compliant có thành công không;
5. read-back byte/content chính xác;
6. unrelated files không bị thay đổi.

---

# Phase 5 — `apply_patch` transaction torture

Mỗi test phải read-back target sau call.

## P1 — multi-file success

Một patch thay ít nhất 5 files.

Tất cả phải thành công atomically.

## P2 — late failure rollback

Patch:

* target 1 valid;
* target 2 valid;
* target 3 invalid/missing context.

Expected:

```text
whole patch FAIL
target 1 unchanged
target 2 unchanged
target 3 unchanged
```

## P3 — duplicate target

Hai update trong cùng patch nhắm cùng path.

Expected:

```text
INVALID_PATCH
no partial mutation
```

## P4 — destination exists move

Move A → B khi B đã tồn tại.

Expected:

* reject;
* A unchanged;
* B unchanged.

## P5 — directory delete through Delete File

Attempt delete directory bằng file-delete patch operation.

Expected:

* reject;
* nested files còn nguyên.

## P6 — mixed line endings

Fixture chứa intentional:

```text
CRLF
LF
CRLF
```

Thực hiện patch có deletion + surviving context + replacement.

Đọc bytes sau patch, không chỉ rendered text.

Xác minh surviving context giữ original EOL/whitespace.

## P7 — very large patch near reasonable limit

Tạo patch lớn nhưng không cố vượt server hard limit quá mức.

Kiểm:

* accept hoặc explicit bounded error;
* không timeout mơ hồ;
* không partial mutation.

---

# Phase 6 — Process / session behavior thực tế

## E1 — ordinary command

Kiểm:

```text
pwd
stdout
stderr
exit 0
exit != 0
```

`completion_reason` phải được interpret cùng `exit_code`.

## E2 — long-running process

Start process:

```text
START
sleep
END
```

Initial response phải trả session khi còn running.

Poll bằng cùng `session_id`.

Không được restart command.

## E3 — finished-but-truncated recovery regression

Sinh khoảng **1–2 MiB presentation output** hoặc đủ để response bị token/presentation truncate nhưng vẫn nằm trong retained byte buffer.

Process phải exit 0 nhanh.

Expected first result:

```text
completion_reason=exited
truncated=true
session_id != null
continuation yêu cầu replay
```

Sau đó cố ý thực hiện **cursorless poll trước**:

```text
write_stdin(session_id)
```

Expected sau fix:

```text
completion_reason=exited
output có thể rỗng
session_id vẫn phải còn
continuation vẫn phải nói recovery còn available
```

Sau đó replay:

```text
write_stdin(
  session_id,
  since_output_offset=<appropriate old cursor>
)
```

hoặc qua `extensions` nếu đó là public route.

Phải recover retained output.

Nếu cursorless poll khiến session/recovery handle biến mất, **FAIL E3**.

## E4 — head+tail evicted-middle cursor regression

Sinh > process retained-buffer limit, ví dụ khoảng 5 MiB nếu default retention ~4 MiB.

Output phải có:

```text
E4_HEAD_UNIQUE
<large middle>
E4_TAIL_UNIQUE
```

Lấy session recovery handle.

Replay từ một cursor chắc chắn nằm trong vùng middle đã bị evict.

Expected:

* response có explicit omission marker;
* `output_offset` phải nhảy tới first retained tail byte;
* có `E4_TAIL_UNIQUE`;
* **không được fabricate/replay `E4_HEAD_UNIQUE`** như thể buffer contiguous;
* cursor semantics phải truthful.

Phân biệt:

```text
evicted bytes unrecoverable
```

với:

```text
cursor algorithm sai
```

## E5 — presentation truncation retry

Replay cùng `since_output_offset` với `max_output_tokens` rất nhỏ.

Sau đó retry **same cursor** với cap lớn hơn.

Expected:

* không cần rerun command;
* output recoverable theo documented semantics.

## E6 — stderr logical-line handling

Raw write:

```text
stderr: ERR
stderr: OR\n
```

Expected logical rendered line tương đương:

```text
[stderr] ERROR
```

không chèn prefix vào giữa logical line.

## E7 — signal lifecycle

Test cả:

1. process tự SIGTERM;
2. requested TERM bị trap rồi process `exit 42`;
3. forced kill nếu safe.

Xác minh:

```text
completion_reason
requested_signal
signal
exit_code
timed_out
deadline_exceeded
```

Không coi `exit_code` đơn lẻ là lifecycle truth.

## E8 — multiple sessions

Start ít nhất 3 harmless sessions song song/interleaved.

Poll chúng theo thứ tự khác với creation order.

Kiểm:

* output không cross-talk;
* session IDs unique;
* signal session A không ảnh hưởng B/C;
* completion của session này không làm mất session khác.

---

# Phase 7 — PTY

Nếu PTY supported:

1. `tty=true`;
2. verify stdin/stdout là TTY;
3. resize rows/cols;
4. gửi interactive input;
5. kiểm terminal snapshot;
6. process tự signal.

Không coi raw ANSI output là corruption nếu PTY contract nói raw stream có escape sequences.

---

# Phase 8 — Podman / external runtime

Chỉ test nếu environment brief nói Podman/container runtime available.

Không đoán alias.

Dùng invocation được environment brief advertise.

Kiểm:

```text
podman info
container run --rm
pwd/environment trong container
```

Nếu interactive `-it`, dùng `tty=true`.

Container phải tên prefix:

```text
tmp-cbrw-
```

Không để container tồn tại sau test.

Nếu direct rootless Podman fail nhưng environment advertise explicit sudo/rootful invocation, test đúng advertised path.

Phân biệt:

```text
environment limitation
```

với:

```text
CodexBridge routing bug
```

---

# Phase 9 — Memory correctness / F2 regression

Trước test xóa mọi key prefix:

```text
codexbridge-rw-e2e-
```

Tạo keys theo lexical order, ví dụ:

```text
codexbridge-rw-e2e-alpha
codexbridge-rw-e2e-beta
codexbridge-rw-e2e-gamma
```

## M1 — direct recall

Exact values phải round-trip.

## M2 — pagination

Gọi:

```text
recall(max_results=1)
```

Expected page đầu có:

```text
snapshot_hash
next_offset
```

## M3 — missing snapshot must fail closed

Continuation:

```text
offset > 0
snapshot_hash absent
```

Expected current behavior:

```text
INVALID_INPUT
```

Không được silently paginate bằng offset trên snapshot mới.

Nếu trả page bình thường → **FAIL critical correctness regression**.

## M4 — valid stable continuation

Gửi exact page-1 hash.

Expected next page đúng, không duplicate/skip.

## M5 — stale snapshot

Sau page 1:

1. insert một key sort trước key đã đọc;
2. continuation bằng **old snapshot_hash**.

Expected:

```text
PAGINATION_STALE
```

Sau đó restart `offset=0` và phải thấy snapshot mới.

## M6 — extension fallback

Nếu `extensions` public:

* gửi `snapshot_hash` qua extension;
* kiểm typed top-level value thắng extension conflicting/invalid;
* known extension sai type → `INVALID_INPUT`;
* unknown future extension không phá request.

---

# Phase 10 — Plan lifecycle

Tạo plan có nhiều steps:

```text
pending
in_progress
completed
```

Kiểm invariant:

* không hơn một `in_progress`;
* recall include plan đúng;
* invalid update không destroy previous committed plan;
* `update_plan([])` clear hoàn toàn;
* final recall `plan=null`.

---

# Phase 11 — Skills / nested scope

Nếu public tools có `skills_list` / `skills_read`:

1. tạo project-local valid skill trong scratch/scope phù hợp;
2. tạo nested skill;
3. nếu safe, tạo malformed skill riêng;
4. verify catalogue vẫn trả valid skills;
5. malformed entry chỉ tạo warning, không poison toàn catalogue;
6. test `skills_read` pagination;
7. test UTF-8 cursor ở emoji boundary;
8. page limit quá nhỏ phải explicit error thay vì cursor loop;
9. verify precedence project/nested/user theo contract được advertise.

---

# Phase 12 — Resource-boundary / unexpected cases

Không cố DoS server. Chỉ test reasonable boundaries.

Thử:

* `max_results=0`;
* very small limits;
* exact limit;
* one over limit;
* offsets ở EOF;
* offsets past EOF;
* unknown extension keys;
* malformed extension known keys;
* empty strings;
* Unicode keys;
* long but allowed paths;
* repeated pagination calls cùng cursor;
* duplicate replay request cùng `since_output_offset`.

Quan sát idempotency/determinism.

Đặc biệt tìm:

```text
same request → different data
cursor không tiến
silent skip
silent duplicate
partial mutation after error
success response với inconsistent metadata
```

---

# Phase 13 — Huge-project repeated workflow soak

Thực hiện ít nhất **3 vòng** workflow sau trên large fixture:

1. search một marker;
2. list một large directory qua pagination;
3. read một long file qua continuation;
4. patch 2–3 files;
5. run command kiểm fixture;
6. remember một result;
7. recall state;
8. revert fixture change bằng một patch khác.

Mục tiêu là phát hiện bug chỉ xuất hiện sau nhiều tool calls:

* stale internal state;
* leaked session;
* incorrect cursor reuse;
* resource exhaustion;
* degraded latency;
* cross-call corruption.

Ghi approximate wall time nếu tool trả.

Không đánh FAIL chỉ vì project lớn chậm hơn; đánh FAIL nếu:

* timeout bất hợp lý;
* session/resource leak;
* inconsistent result;
* traversal stops mà không báo incomplete;
* pagination không progress;
* mutation sai.

---

# Phase 14 — Cleanup

Cleanup phải chạy dù test trước đó FAIL.

Xóa:

```text
codexbridge-realworld-e2e/
```

Xóa mọi memory key:

```text
codexbridge-rw-e2e-*
```

Clear plan:

```text
update_plan([])
```

Xóa container/resource prefix:

```text
tmp-cbrw-
```

Sau đó verify bằng public tools:

* scratch → `FILE_NOT_FOUND`;
* prefixed memory keys không còn;
* `plan=null`;
* prefixed containers = 0.

Không xóa bất kỳ resource không có prefix test.

---

# Cách đánh PASS / FAIL

Một case chỉ PASS khi có observable evidence.

**PASS**

Behavior đúng contract và read-back/evidence xác nhận.

**FAIL**

Bao gồm bất kỳ:

* silent incorrect result;
* partial mutation;
* duplicated/skipped pagination result;
* cursor stuck;
* inconsistent cursor metadata;
* continuation được advertise nhưng không thể sử dụng qua public contract;
* output recovery mất ngoài documented retention behavior;
* stale snapshot không bị phát hiện ở nơi contract yêu cầu;
* scope/instruction guard xảy ra sau mutation;
* process/session cross-talk;
* tool crash/panic;
* malformed data làm poison unrelated operations;
* cleanup leak.

**SKIP**

Chỉ dùng khi capability thật sự unavailable trong environment/platform.

Không dùng SKIP để che failure.

---

# Severity

Mỗi FAIL phải có severity:

```text
CRITICAL
HIGH
MEDIUM
LOW
```

Ưu tiên HIGH cho các lỗi kiểu:

```text
silent data corruption
partial mutation
wrong project/session isolation
rerun-risk của state-changing command
silent memory pagination duplicate/skip
filesystem boundary violation
```

---

# Output bắt buộc

Cuối cùng trả **một báo cáo duy nhất**, không sửa production source.

Bắt đầu bằng bảng:

| Case | PASS/FAIL/SKIP | Evidence ngắn | Error/code | Severity nếu FAIL |
| ---- | -------------- | ------------- | ---------- | ----------------- |

Case IDs nên giữ rõ:

```text
D1-D4 discovery
L1-L4 large project
F1-F5 filesystem
W1 real workflow
P1-P7 patch
E1-E8 process
T1 PTY
C1 Podman
M1-M6 memory
PL1 plan
S1-S9 skills
R1 resource boundaries
SOAK1 repeated workflow
CL1 cleanup
```

Sau bảng, bắt buộc có:

## Critical findings

Chỉ các lỗi có impact thực tế.

Với mỗi finding ghi:

```text
Observed behavior
Expected behavior
Minimal reproduction
Impact
Likely layer:
  CodexBridge runtime
  CodexBridge MCP schema
  connector/model-facing schema
  environment
Confidence
```

## Unexpected but non-critical behavior

Ghi những khác biệt đáng chú ý nhưng không đủ để FAIL.

## Large-project assessment

Trả lời cụ thể:

* 10k–15k files có làm tools sai không?
* pagination còn deterministic không?
* search có silently incomplete không?
* memory/process/session state có degrade sau nhiều calls không?
* latency/resources có dấu hiệu bất thường không?
* có operation nào không scale thực tế không?

## Real coding-agent assessment

Đánh giá CodexBridge khi dùng thực tế:

* agent có nguy cơ hiểu sai state không;
* agent có nguy cơ rerun command đã chạy không;
* agent có thể bỏ sót file/result vì pagination không;
* errors có đủ explicit để agent recover không;
* continuation APIs có usable từ public schema không;
* nested instructions có enforce trước mutation không;
* multi-file edits có đủ an toàn không.

## Cleanup status

Liệt kê evidence rằng toàn bộ fixture đã được xóa.

Cuối cùng ghi:

```text
Overall verdict:
READY / READY WITH LIMITATIONS / NOT READY

Top 3 risks:
1.
2.
3.
```

Không sửa bug trong lượt test này. Chỉ test, reproduce, phân loại và báo cáo.
