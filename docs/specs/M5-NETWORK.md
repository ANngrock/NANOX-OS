# M5: сеть, ключи и нативный API provider

Статус: контракт этапа. Реализованы host-проверяемые срезы M5-1
(`crates/net-wire`), M5-2 (DNS codec в `net-wire::dns`), M5-3
(`crates/net-tcp`, ограниченный TCP-профиль) и M5-4 (`crates/net-stack`,
ARP/DNS transport/demux). Это не завершает этап: ни один критерий M5 в
[ROADMAP](../ROADMAP.md) не выполнен.

## 1. Порядок работ

По указанию пользователя M5 начат раньше M1–M4. Законченной функцией M5
становится только после них; до этого разрабатываются части, которые
проверяются без ядра и userspace.

| Зависимость | Этап | Для чего нужна M5 |
| --- | --- | --- |
| Физический allocator, mapping, heap | M1 | DMA-буферы и очереди virtio-net, буферы пакетов |
| Timer и trace IDs | M1 | Retransmit, ARP/DNS timeouts, TLS time policy |
| Userspace, handles, IPC | M2 | Сетевой сервис и драйвер вне ядра, выдача прав Core |
| Durable object store | M4 | API credentials, trust store, конфигурация сети |
| Core с полномочиями Sovereign | M2–M3 | Вызов provider из NANOX |

Без этих частей нельзя заявлять драйвер, TCP, TLS или вызов provider.

## 2. Слои

1. `crates/net-wire` — чистые кодеки кадров и заголовков (M5-1) и DNS-кодек
   stub resolver (M5-2). `crates/net-tcp` — кодек TCP и клиентское
   соединение ограниченного профиля (M5-3, [M5-TCP](M5-TCP.md)).
2. `crates/net-stack` — host-проверяемые bounded структуры и single-interface
   демультиплексор; это не userspace service и не интеграция с NIC.
3. Драйвер virtio-net в userspace: очереди, DMA, link state.
4. Сетевой сервис: драйвер и IPC, ARP cache, IPv4 routing, ICMP echo, UDP
   sockets, DNS stub, TCP и timers.
5. Entropy/CSPRNG, криптография и TLS 1.3 с проверкой сертификатов.
6. Клиент provider с потоковыми ответами и восстановлением соединения.

## 3. Контракт `crates/net-wire` (M5-1)

- `no_std`, без `alloc`, `#![forbid(unsafe_code)]`, без сторонних crates.
- Парсеры читают только после проверки длины. Данные переменной длины
  (payload, IPv4 options, данные ICMP и UDP) возвращаются как ссылки на
  буфер вызывающего; поля фиксированного размера (адреса, порты, весь
  `ArpPacket`) копируются в значения. Некорректный вход — `WireError`, не
  panic.
- Эмиттеры пишут в буфер вызывающего; нехватка места — `BufferTooSmall`,
  выход длины за 16-битное поле — `Overflow`.

### Политика по протоколам

**Ethernet.** Только Ethernet II. EtherType < 0x0600 (802.3), 0x8100 и
0x88a8 (VLAN) — `UnsupportedFrame`. Групповой адрес источника —
`BadAddress`. Payload включает padding; следующий уровень ограничивает себя
своим полем длины. Эмиттер не дополняет кадр до 60 байт — это задача
драйвера.

**ARP.** Только htype 1, ptype 0x0800, hlen 6, plen 4, операции request и
reply; иначе `UnsupportedArp`. Групповой MAC или групповой/broadcast IPv4
отправителя — `BadAddress` и при разборе, и при эмиссии. Адреса цели не
ограничиваются.

**IPv4.** Порядок проверок: длина ≥ 20; version 4 (`BadVersion`);
IHL ≥ 5 (`BadHeaderLength`); заголовок помещается во вход (`Truncated`);
total length ≥ IHL·4 (`BadLength`) и помещается во вход (`Truncated`);
checksum заголовка (`BadChecksum`); reserved flag (`ReservedFlag`);
фрагменты; групповой источник (`BadAddress`). Байты после total length —
padding канала, в payload не входят.

- *Options:* проверяются только по длине через IHL и покрываются checksum;
  отдаются как сырые байты и не интерпретируются. Source route и прочие
  опции не выполняются: форвардинга нет.
- *Fragments:* любой пакет с MF или ненулевым offset — `Fragmented`.
  Сборки фрагментов в M5-1 нет. Если она понадобится, она появится в
  сетевом сервисе отдельным срезом с лимитами памяти, времени и числа
  незавершённых датаграмм и с тестами перекрытий.
- Эмиттер: заголовок 20 байт без options, DF установлен, TTL 0 запрещён
  (`InvalidField`), total length ≤ 65535.

**ICMP.** Только echo request/reply с code 0; прочее с верной checksum —
`UnsupportedIcmp`. Checksum покрывает всё сообщение.

**UDP.** Длина ≥ 8 (`BadLength`) и помещается в IPv4 payload
(`Truncated`); лишние байты отбрасываются. Нулевая checksum на приёме
допустима (IPv4), иначе проверяется с pseudo-header. Порт назначения 0 —
`BadPort`. Эмиттер всегда вычисляет checksum; вычисленный 0 передаётся как
0xffff.

## 4. Проверки M5-1

`crates/net-wire/tests/wire.rs`, запускается `cargo xtask test` в составе
host-тестов:

- пример RFC 1071 и эталонный IPv4-заголовок; checksum по частям нечётной
  длины;
- round trip всех кодеков (ICMP echo request и reply) и цепочка
  Ethernet → IPv4 → UDP;
- групповой и broadcast IPv4-источник отвергаются, broadcast-назначение
  принимается;
- каждое правило политики выше отдельным негативным случаем: IHL 0–4,
  IHL больше входа, total length меньше заголовка и больше входа, checksum,
  reserved/MF/offset, options под checksum, VLAN (0x8100, 0x88a8) и 802.3,
  неверные поля ARP, групповые MAC/IPv4 отправителя ARP при разборе и эмиссии,
  неподдержанный ICMP, длина/checksum/порт UDP, лимиты эмиттеров;
- любое усечение каждого формата даёт ошибку;
- 20 000 детерминированных псевдослучайных входов не вызывают panic;
- сборка для `x86_64-unknown-none` подтверждает `no_std`.

## 5. Контракт DNS (M5-2, `net_wire::dns`)

Stub-запрос A/IN поверх UDP (RFC 1035, RFC 2181); тот же `no_std`/без
`alloc`/без `unsafe`. Транспорт, повторы, таймауты и выбор сервера — задача
сетевого сервиса.

- **Имя запроса** (`DnsName::from_text`): метки 1–63 байта из букв, цифр и
  внутренних дефисов (RFC 1123), завершающая точка допустима, всё имя в
  wire-форме с корневой меткой ≤ 255 байт. Иначе `BadName`/`NameTooLong`.
  Сравнение имён — без учёта регистра ASCII.
- **Запрос** (`emit_a_query`): заданный ID, только RD, один вопрос A/IN.
- **Проверка ответа** (`parse_a_response`), по порядку: ID (`IdMismatch`);
  QR=1, opcode 0, QDCOUNT=1 (`BadHeader`); сумма ANCOUNT+NSCOUNT+ARCOUNT ≤ 64
  (`TooManyRecords`); вопрос совпадает с запросом по имени, типу и классу
  (`QuestionMismatch`) — до того, как верить флагам и RCODE; TC
  (`TruncatedResponse`, нужен TCP, которого нет); RCODE 2/3/5 и прочие —
  отдельные ошибки.
- **Выбор результата.** Результат — только A/IN-записи секции Answer,
  достижимые от QNAME через цепочку CNAME длиной не более 8
  (`CnameChainTooLong`). Authority и Additional проходят те же проверки
  структуры и RDATA известных IN-типов (A, CNAME), что и Answer, но в
  результат не попадают никогда; прочие типы (например, OPT) пропускаются. Записи
  других классов, типов и владельцев игнорируются. Цикл CNAME —
  `CnameLoop`; разные CNAME одного владельца или CNAME вместе с A —
  `CnameConflict`; одинаковые дубликаты CNAME допустимы.
- **Выход.** Адреса пишутся в массив вызывающего; если достижимых записей
  больше его длины — `OutputFull`: ёмкость проверяется до записи, массив
  вызывающего остаётся неизменным.
  `count = 0` при RCODE 0 — NODATA. TTL результата — минимум по всем
  пройденным CNAME и возвращённым A; TTL со старшим битом считается 0.
- **RDATA.** A/IN — ровно 4 байта; CNAME — ровно одно имя; RDLENGTH за
  пределами сообщения — `Truncated` (`BadRdata` для прочего).
- **Сжатие.** Указатель должен вести ниже начала серии меток, в которой он
  стоит, поэтому позиции строго убывают и декодирование завершается; иначе
  `BadPointer`. Типы меток 0x40/0x80 — `BadName`. Лимит 255 байт действует
  и для имён, собранных через указатели.

Тесты `crates/net-wire/tests/dns.rs` (14): точные байты запроса; валидация
имён и граница 255 байт; эталонный ответ со сжатием; каждая ошибка
заголовка, вопроса и RCODE, в том числе «NXDOMAIN» на чужой вопрос;
Authority/Additional и посторонние владельцы не дают результата; цепочка
CNAME и минимум TTL, NODATA через CNAME; цикл, конфликты, длина цепочки
8/9; `OutputFull` без изменения выходного массива (в том числе за CNAME);
RDATA во всех трёх секциях, пропуск OPT; указатели вперёд, на себя, за конец, петля через
собственную серию, зарезервированные типы меток; все усечения; 20 000
псевдослучайных сообщений без panic.

## 6. Host-профиль сетевого сервиса (`crates/net-stack`, M5-4)

Этот crate зависит только от собственных `net-wire` и `net-tcp`; он
`no_std`, без `alloc` и `unsafe`. Ввод-вывод и монотонные ticks остаются за
вызывающим кодом. Компонент не является NIC-драйвером и пока не запускается
в kernel/userspace.

**ARP cache.** Фиксированная ёмкость задаётся const-параметром. Lookup
создаёт pending-запись и запрашивает ARP; число попыток и задержка задаются
конфигурацией, повтор/timeout выдаются вызывающему по внешним ticks.
Доступны записи `Reachable` и `Pending`; живые и pending записи не
вытесняются при заполнении таблицы. Истёкшие Reachable-записи освобождают
слот. Повторное наблюдение той же пары IP/MAC обновляет срок, конфликтующая
пара не перезаписывает существующую запись и возвращает `Conflict`; если
слота нет, возвращается `TableFull`. Явное обучение unsolicited ARP
разрешено. ARP не аутентифицирует отображение IP–MAC: конфликтное правило
сохраняет текущую запись, но не является защитой от подмены.

**DNS transport.** `net_stack::dns::DnsClient` ведёт один запрос A/IN поверх
UDP за раз. Caller задаёт `DnsName`, DNS server, local port, начальный ID seed
и время. Выходной DNS payload строится в caller buffer для source port,
server port 53; повторы используют новый детерминированно вращаемый ID и
экспоненциальный backoff до лимита попыток. Вход принимается только от
настроенного server:53 на local port и с ID текущей попытки. Некорректные,
чужие и устаревшие пакеты не завершают запрос; RCODE и TC сообщаются
отдельно. ID seed не является энтропией, source-port randomization и
защита от подделки ответов ещё требуют CSPRNG/политики M5. TCP fallback для
TC, параллельные запросы, выбор нескольких серверов и AAAA не реализованы.

**IPv4 demux.** `demux_frame` обрабатывает один `Interface`: ARP принимается
только для локального target IP, с MAC sender, совпадающим с Ethernet
source; IPv4 — только для локального unicast MAC/IP и TTL != 0. IPv4
фрагменты отклоняются codec-ом. ICMP echo, UDP и TCP разбираются
соответствующими codecs (в том числе их checksums); неизвестные EtherTypes
и IP protocols игнорируются. Структурно неверный адресованный пакет
возвращает ошибку, не panic. Routing, forwarding, сокеты и отправка ответов
не входят в этот host-срез.

**Детерминированный симулятор.** `crates/net-stack/tests/simulation.rs`
пропускает ARP request/reply, TCP handshake, двусторонние данные и active
close через `demux_frame` и реальный `net_tcp::Connection`. Seeded link
имеет clean и fault-профили; fault-профиль отбрасывает первый SYN,
дублирует повторный SYN и data, а также переставляет data перед задержанным
handshake ACK. Это тестовый harness, не production network component; он
проверяет stack/TCP integration host-only, а не guest/реальное устройство.

## 7. Что по-прежнему не входит в host-срезы

Virtio-net driver и DMA, интеграция с kernel/userspace/IPC и аппаратным
таймером, маршрутизация и sockets, TCP passive-open и неподдержанный профиль
из [M5-TCP](M5-TCP.md), CSPRNG, crypto/TLS 1.3, trust store, durable key
storage и вызов реального provider. Сеть в QEMU отсутствует: M0 profile
отключает NIC, поэтому QEMU replay проверяет только отсутствие регрессий M0.
