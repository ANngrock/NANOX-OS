# Объекты, syscall ABI и NCI

Статус: baseline семантики. Handoff v1 фиксируется в M0; syscall schema и её generated types реализуются в M2; NCI bridge — в M3. Дальние API не замораживаются без реализации.

## 1. Три разных границы

| Граница | Представление |
| --- | --- |
| Loader → kernel | BootInfo v1, общий `repr(C)` crate |
| Userspace → kernel | x86-64 syscall ABI, фиксированные числа и проверяемые buffers |
| Core/bridge → services | Versioned NCI messages, сериализация и typed effects |

Общие типы и названия должны согласовываться, но HTTP provider schema не становится syscall ABI. Kernel не содержит JSON parser.

## 2. Идентичности

- `Handle`: u64; младшие 32 бита slot, старшие 32 — generation; 0 invalid. Значение локально для process handle table.
- При закрытии slot generation увеличивается; при исчерпании generation slot выводится из повторного использования до завершения процесса. Старый handle не оживает после wrap.
- `ObjectId`: boot epoch + kernel object counter; для наблюдений, не для авторизации.
- `ObjectRevision`: возрастающая версия изменяемого состояния владельца.
- `PersistentObjectId`: идентичность в хранилище, независимая от хеша версии.
- `ContentHash`: хеш точного payload с указанным алгоритмом.
- `RequestId`: идентичность операции; `AttemptId` — одной попытки.

Kernel object живёт, пока существуют references и необходимые внутренние удержания. Close handle не означает немедленное уничтожение объекта, на который ссылаются другие задачи или незавершённое IPC.

## 3. Права

Rights mask u64, baseline bits: READ, WRITE, EXECUTE, MAP, DUPLICATE, TRANSFER, WAIT, SIGNAL, INSPECT, MANAGE. Точные bit numbers назначаются единожды в `abi/objects.toml` в M2; неизвестные bits отвергаются.

Операция проверяет и object type, и необходимое право. Duplicate/transfer обычного handle даёт только подмножество прав. Sovereign authority — отдельный kernel object, доступ к которому позволяет выполнять полные системные операции и выдачу прав. Это не пользовательский boolean внутри payload.

Начальный transfer — COPY с подмножеством прав. MOVE появится после определения семантики in-transit владения. До этого невозможный move не эмулируется ненадёжной последовательностью «copy и close».

## 4. Machine syscall convention

Использовать инструкцию `SYSCALL`. Номер — RAX; шесть аргументов — RDI, RSI, RDX, R10, R8, R9. RCX/R11 считаются clobbered. При успехе RAX — код 0, RDX — скалярный результат; при ошибке RAX — отрицательный error code, RDX = 0. Большие результаты возвращаются через проверенный output buffer.

Первый return path — проверенный `IRETQ` frame. Оптимизация до `SYSRET` требует отдельной проверки canonical addresses, flags и privilege rules. Нельзя исполнять Rust-код на user stack; entry сначала сохраняет нужное состояние и переключает стек по доверенному per-CPU state.

Первые syscalls, числовые ID фиксируются генератором в M2:

| Операция | Назначение |
| --- | --- |
| `abi_query` | Версия, feature bits, размеры и лимиты |
| `handle_close` | Закрыть свою ссылку |
| `handle_duplicate` | Получить handle с подмножеством прав |
| `object_query` | Прочитать свойства доступного объекта |
| `object_invoke` | Выполнить типизированную операцию объекта |
| `ipc_call` | Отправить bounded request и дождаться reply/deadline |
| `ipc_receive` | Получить request/handles и одноразовый reply token |
| `ipc_reply` | Ответить по reply token |
| `notification_wait` | Ожидать signal mask/deadline |
| `notification_signal` | Установить сигналы |
| `memory_map` | Отобразить доступный MemoryObject |
| `memory_unmap` | Удалить свой mapping |
| `thread_yield` | Передать execution другим runnable threads |
| `thread_exit` | Завершить текущий thread |

Bootstrap creation и полный контроль доступны через `object_invoke` над Authority. Разрастание универсального invoke в непроверяемый switch предотвращается схемой object type/opcode/request/response и сгенерированной dispatch metadata.

До allocation проверять count/length/overflow. Для copyin/copyout определить recoverable fault path; не полагаться только на проверку указателя перед обращением. Output buffer и частичный результат имеют документированную семантику при ошибке.

## 5. IPC baseline

- До 256 байт inline payload и до 4 handles на сообщение M2.
- Большие данные — отдельный MemoryObject с явными правами.
- Deadline абсолютный по monotonic clock; значение infinity задаётся схемой.
- Receive создаёт одноразовый reply token, связанный с ожидающим call.
- Token не используется для произвольного ответа другой задаче.
- При смерти peer разблокировать ожидание с `PEER_DIED`.
- При timeout caller прекращает ожидание, но это не доказывает отсутствие эффекта у server.
- Передача COPY handles и публикация request имеют определённую точку commit; до неё ошибка освобождает созданные references.
- Циклические synchronous calls могут создать deadlock: сервисные протоколы избегают цикла, тесты используют deadlines. Не обещать обнаружение всех deadlocks.
- Notifications — уровень/маска состояния с документированным clear, не замена очереди всех событий.

Проектные лимиты меняются через schema version/features, не произвольными константами в разных сервисах.

## 6. NCI envelope

Иллюстрация содержимого после декодирования; это не авторизационный документ:

```json
{
  "schema": "nci.action.v1",
  "request_id": "req-1042",
  "operation": "task.terminate",
  "target": { "boot_epoch": 17, "object_id": 481, "revision": 19 },
  "arguments": { "reason": "user_request" },
  "deadline_ms": 2000
}
```

Владелец объекта проверяет revision перед мутацией. Подтверждённый transport/caller identity хранится отдельно и не берётся из поля сообщения. Наличие ObjectId не выдаёт handle на объект.

Для каждого operation schema задаёт вход, выход, тип target, требуемое право, effect class, идемпотентность, ошибки, deadline/cancel и способ проверки результата.

## 7. Классы эффектов

| Класс | Семантика |
| --- | --- |
| OBSERVE | Чтение без намеренной мутации объекта |
| VOLATILE_MUTATION | Изменение живого состояния, возможна потеря после reboot |
| DURABLE_MUTATION | Изменение, подтверждённое владельцем как сохранённое |
| EXTERNAL_EFFECT | Сеть/устройство/внешняя система; локальный rollback не отменяет эффект |
| SYSTEM_TRANSITION | Trial boot, reboot, смена поколения, authority/configuration transition |

Это описание эффекта, не ограничение прав Core. При нескольких эффектах схема перечисляет их все.

Idempotency задаётся отдельно: NONE, NATURAL или DEDUPLICATED. Дедупликация хранится у владельца эффекта и атомарно связывается с изменением там, где это возможно. Host bridge не может гарантировать exactly-once сам по себе.

Обязательные исходы: SUCCESS, INVALID_ARGUMENT, BAD_HANDLE, DENIED, UNSUPPORTED, CONFLICT, NOT_FOUND, TIMEOUT, CANCELLED, PEER_DIED, RESOURCE_EXHAUSTED, IO_ERROR, OUTCOME_UNKNOWN. `DENIED` применяется к реально недостаточным правам обычного caller; не имитирует скрытый veto над Sovereign.

## 8. COM2 bridge v1

Выбран простой frame: 4-byte magic `NXCI`, version u16, kind u16, payload_length u32, sequence u64, затем UTF-8 JSON payload. Заголовок сериализуется по полям little-endian, без зависимости от Rust padding. Размер header 20 байт, максимум payload 64 KiB.

Kinds: HELLO, REQUEST, RESPONSE, EVENT, CANCEL, ERROR; числовые значения закрепляются в schema M3. Ограничить queue length, время сборки frame и размер JSON nesting. Неверная magic/version/length закрывает сессию с диагностикой; бесконечный скан мусора не допускается.

Sequence обнаруживает пропуски в сессии, но не служит криптографической аутентификацией. COM2 подключается к локальному harness-controlled endpoint без public listener. Для удаления bridge на другую машину нужен отдельный аутентифицированный transport.

HELLO согласует schema/version/features, boot epoch и session ID. Обрыв посреди frame не запускает операцию. Повтор после reconnect использует request status, а не слепое переисполнение.

Provider API располагается за bridge на host. Guest actions исполняет NANOX executor; host не должен выполнять произвольную shell-команду из guest request.

## 9. Обязательные conformance tests

- Golden wire bytes и roundtrip для допустимых сообщений.
- Truncation, length overflow, unknown enum/opcode/version и лишние права.
- Stale/reused handles и cross-process handle confusion.
- Timeout до доставки, во время выполнения и после эффекта до ответа.
- Peer death и очистка references/очередей.
- Дублированный request и конфликт revision.
- Неправильный provider tool-call payload не исполняется.
- Ответ об успехе проверяется по состоянию ресурса.

Точные syscall signatures добавляются вместе с реализацией M2 и проверяются между kernel/abi-user. Это осознанная граница baseline, а не обещание готового ABI всех будущих сервисов.
