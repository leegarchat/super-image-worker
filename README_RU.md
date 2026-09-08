# super-image-worker

> Высокопроизводительная автономная Rust-утилита для глубокой инспекции, модификации и генерации Android-образов раздела `super`: LP-метаданные (`liblp`, динамические разделы), raw- и Android Sparse-контейнеры, одно- и мультиблочные split/retrofit-раскладки, плюс встроенный аналог `lpmake`.

Утилита полностью автономна: для всей офлайн-работы не нужны права суперпользователя, монтирование в ядро и внешние хостовые бинарники (`lpmake`, `lpunpack`, `simg2img`). Root нужен только командам `connect`/`disconnect` (loop-устройства) и `map`/`unmap` (device-mapper, только Android/Recovery) — всё остальное суть обычный пользовательский файловый ввод-вывод. Все блочные операции и работа с метаданными идут потоком с потреблением RAM $O(1)$ и никогда не загружают образы целиком в память.

---

## Ключевые возможности

* **100% автономный user-space**: прямой парсинг, мутация и генерация LP-структур на чистом безопасном Rust. `lpmake`/`lpunpack`/`simg2img` не требуются.
* **Политика нулевой паники**: никаких `unwrap()`, `expect()` и паникующих индексаций во всех production-путях `super-image-worker-core`. Все бинарные структуры проверяются границами и возвращают `Result<T, Error>`.
* **Строгая дисциплина памяти ($O(1)$ RAM / потоковость)**: чтение, запись, извлечение и генерация образов идут фиксированными чанками 1–4 МиБ. Многогигабайтные payload (например, `system.img` на 2–4 ГБ) никогда не лежат в RAM целиком — безопасно на 32-битных целях и RAM-дисках.
* **Целостность SHA-256 + автоматический fallback**: контрольные суммы геометрии, заголовка и таблиц проверяются при каждой загрузке. Поврежденная первичная геометрия/метаданные автоматически подменяются резервными копиями (`0x2000` / backup-слоты).
* **Послотно-безопасная запись**: обновление метаданных перезаписывает primary- И backup-копии только загруженного слота (`Backup(slot) = 0x3000 + (slot_count + slot) * max_size`); соседние слоты не затрагиваются никогда.
* **Безопасный от наложений `resize`**: рост расширяет последний экстент на месте, только если хвост доказанно свободен, иначе выделяет новый линейный экстент и вшивает его в окно раздела (последующие разделы сдвигаются) — блоки соседа не затираются.
* **Split / Retrofit Super**: мультиблочные раскладки (`block_devices`, маршрутизация через `target_source`) на нескольких файлах через повторяемый `--device` (`name=path` либо автоматч по имени файла).
* **Встроенный аналог `lpmake` (`make`)**: генерация образов с нуля с произвольным целевым слотом (`0|1|a|b|all` — штатный `lpmake` пишет только слот 0), одиночным или retrofit-выводом, raw- или Sparse-контейнерами, мультиблочными spanning-экстентами, когда на одном устройстве места не хватает, пинингом разделов на устройство (`group@device`), переопределением выходов (`--output-map`, файлы или raw-блочные узлы) и сборкой сразу в блочное устройство (размер из `--device` обязан совпасть с probed-размером блока; выводы предварительно зануляются).
* **Разделение слот/суффикс**: `-s/--slot` (`0|a`, `1|b`, …, `all`) выбирает, какая копия LP-метаданных используется; `--suffix` (`a|b|all`, только длинный флаг) фильтрует буквы имен внутри нее. Слот метаданных с записями и `_a`, и `_b` читается как `--slot 0 --suffix b`.
* **Raw-блочные устройства везде**: все команды принимают узлы `/dev/block/by-name/*` напрямую (размер определяется через `SEEK_END`, т.к. метаданные блока сообщают 0).
* **Legacy однослотовые super**: раскладки без суффиксов (`system`, `vendor`, …) с одним слотом метаданных полностью поддерживаются генерацией и всеми командами чтения/записи.
* **OTA-снапшот хелперы**: `cow` удаляет `*-cow`-разделы группы `cow` (аналог `lptools --clear-cow`) с гейтом по замапленным девайсам; `snapshot-status` печатает `Update state: <state>` (аналог `snapshotctl dump`) из он-диск стейта снапшотов для ветвлений инсталлера.
* **Многопоточное извлечение**: параллельный `extract` на `rayon`, у каждого потока изолированный дескриптор образа (гонок `Seek` нет).
* **CLI для скриптов**: `--slot`/`--suffix` во всех командах, форматы `human`/`json`/`tsv`/`env`, извлечение скаляров через `--get` без завершающего перевода строки, потоковый `read` с `--skip`/`--size` и корректной обработкой `BrokenPipe`.

---

## Архитектура репозитория

```text
Cargo.toml                  Корень workspace (members + централизованный [profile.release]: LTO fat, abort, strip)
build.sh                    Статический мультиархитектурный сборщик musl (x86_64/x86/aarch64/armv7) через cargo или cross
dist/                       Предсобранные статические бинарники (super-image-worker-linux-*)
crates/super-image-worker-core/   Библиотека без паник: парсинг/аллокация/сериализация:
  src/format/mod.rs         LP-константы, структуры таблиц, AOSP-хелперы смещений слотов
  src/reader/source.rs      Raw- / Android Sparse-образы, поиск чанков O(log N)
  src/reader/parser.rs      Загрузчик геометрии + метаданных с SHA-256-валидацией и fallback
  src/reader/extent_stream.rs   Потоковый ридер экстентов + экстрактор для одного файла
  src/reader/multiblock.rs  Split/retrofit-контейнер (маршрутизация target_source, привязка --device)
  src/reader/mod.rs         Модель SuperData, слот-/групповые хелперы, load_super
  src/writer.rs             Аллокатор (alignment + alignment_offset), рендер слотов, потоковая запись
  src/sparse.rs             Создание файлов заданного размера + потоковый raw->sparse-кодировщик
crates/super-image-worker-cli/    Бинарный крейт (clap derive):
  src/main.rs               Диспетчер команд + глобальная справка
  src/commands/             info, extract, add, resize, remove, rename, create,
                            read, connect, map, make (+ общий хелпер split_util)
  src/output/               Рендеры human_size + human/json/tsv/env/get
```

---

## Инженерные особенности и устранение типовых дефектов LP-инструментов

`super-image-worker` закрывает целый ряд повсеместных дефектов наивных LP-утилит:

| Специфика / особенность формата | Типовой дефект реализаций | Реализация `super-image-worker` |
|---|---|---|
| **Раскладка backup-слотов** | Backup пишется по `offset + max_size`, затирая primary слота B. | Правило AOSP `Backup(slot) = 0x3000 + (slot_count + slot) * max_size`; изоляция слотов проверена хешами. |
| **Рост при `resize`** | `num_sectors` последнего экстента тупо увеличивается, затирая следующий физический раздел. | Расширение на месте только по доказанно свободному хвосту (`is_range_free`); иначе новый экстент вшивается в окно со сдвигом индексов. |
| **RAM под payload** | `fs::read(payload)` грузит многогигабайтные образы целиком → OOM/SIGKILL. | `copy_payload_stream`/`copy_payload_segment` чанками по 1 МиБ; `fs::read` бинарных данных нет нигде. |
| **Границы диска** | На переполненном образе возвращается сектор за пределами диска. | Жесткий лимит `device.size / 512` → `not enough free space on block device`. |
| **`alignment_offset`** | Сохраняется в метаданные, но игнорируется при аллокации (неверная фаза erase-блока на экзотических eMMC/UFS). | `(sector·512) % alignment == offset % alignment` в `align_up`; точно для сектор-кратных геометрий. |
| **Фрагментированный `connect`** | Loop-устройство размером с раздел по смещению первого экстента захватывает чужие блоки. | Мультиэкстентные разделы отклоняются с подсказкой про `map`; требуется ровно один linear-экстент. |
| **Гейтинг `map`** | Разрешен/запрещен не на той ОС (dm-linear на Linux требует loop-базу). | Строго Android/Recovery (`/system/build.prop`, `/sbin/recovery`, `/system/bin/getprop`, `/dev/block/mapper`); хостовому Linux указывает на `connect`. |
| **Чтение sparse** | Линейный поиск чанка — O(N) на чтение, stall на 10k+ чанков. | Бинарный поиск по логическому диапазону — O(log N). |
| **Контрольные суммы** | Хеши геометрии/заголовка/таблиц не проверяются; битые копии доверяются. | Все три SHA-256 валидируются; при несовпадении — fallback primary→backup. |
| **Один `make` на устройство** | Раздел больше любого отдельного устройства роняет сборку, хотя суммарно места хватает. | `plan_spanning_allocation` режет payload в цепочку по устройствам (лимит `largest_free_run`). |
| **Непривязанная запись** | Payload аллоцируется на устройство, чей файл не передан. | `add` аллоцирует только на привязанных (`find_free_sectors_any_in`); быстрый отказ с подсказкой `--device`. |
| **Переполнение групп** | `maximum_size` игнорируется в `add`/`create`/`make`. | Везде проверяется `used + needed ≤ max`, обход только через `--force`. |
| **Смешение слот/суффикс** | Один флаг означает и копию метаданных, и буквы имен — строки `_b` в метаслоте 0 недостижимы. | `-s/--slot` выбирает копию метаданных (индекс или алиас `a`/`b`), `--suffix` фильтрует буквы; `--slot 0 --suffix b` их достает. |
| **Входы-блочные устройства** | `metadata().len()` для блочных узлов равен 0 — все инструменты падают на `/dev/block/...`. | Размер определяется через `SEEK_END`; живые разделы `super` читаются напрямую. |
| **`map` файловых образов** | Путь файла уходит прямо в `DM_TABLE_LOAD` → `ENODEV`. | Обычные файлы автоматически backing-ятся whole-file loop; блочные узлы проходят каноникализированными. |
| **Loop-узлы на Android** | Жесткий `/dev/loopN`, которого нет там, где есть только `/dev/block/loopN`. | Пробуются оба префикса; fallback на свободный узел, если `GET_FREE` указал мимо предсозданных. |

---

## Защитные лимиты целостности (Hard Caps)

Против DoS, неограниченного роста памяти и арифметических повреждений на враждебных входах действуют строгие лимиты:

* **Максимум `metadata_max_size`**: 16 МиБ; **слотов метаданных**: 1–8; **таблиц**: 65 536 записей на таблицу, максимум 16 блочных устройств.
* **Максимум `header_size`**: 1 МиБ; **`tables_size`**: 64 МиБ; **sparse-чанков**: 1 000 000; **sparse-блок**: 512 Б–1 МиБ.
* **Потоковые буферы**: 1 МиБ на payload I/O, окна sparse-кодирования 4 МиБ, максимум RAW-рана 8 МиБ — пиковый RSS не зависит от размера образа.
* **Арифметика**: все секторные/байтовые/смещенные вычисления через `checked_*`/`saturating_*`; чтения записей таблиц валидируются диапазоном против размера образа.
* **Sparse-вывод**: итоги RAW-чанков упираются в `u32::MAX`; `DONT_CARE`-раны соответственно разбиваются.

---

## Сборка и компиляция

**Требования**:
* Тулчейн Rust (stable, edition 2024).
* Менеджер пакетов Cargo.
* Для статических сборок: musl-тулчейны или `cross` (подсказки по установке под каждый дистрибутив — в `build.sh --help`).

```bash
# Клонирование репозитория
git clone https://github.com/leegarchat/super-image-worker.git
cd super-image-worker

# Быстрая локальная сборка (хостовый таргет, динамическая)
cargo build --release
# -> target/release/super-image-worker (~1.2 МБ, LTO + stripped)

# Статические мультиархитектурные сборки через build.sh (musl, stripped):
#   --cargo | --cross | --auto   метод сборки (auto = cross при наличии контейнеров, иначе cargo)
#   --arch all|x64|x86|arm64|arm32
./build.sh --cargo --arch x64      # x86_64-unknown-linux-musl
./build.sh --cross --arch arm64    # aarch64 через контейнеры
./build.sh --arch all              # x86_64, x86, aarch64, armv7

# Проверки
cargo clippy --all-targets -- -D warnings
cargo test
```

Выходы сборки:

* `dist/` — статические бинарники `super-image-worker-linux-*` (x86_64, x86, arm64, arm32).
* `target/push/` — push-копии `{name}_{arch}` (`x64`, `x86`, `arm64`, `arm32`), обновляются под собранную архитектуру, например для устройств:
  `adb push target/push/super-image-worker_arm64 /data/local/` (`dist/` и `target/` в gitignore).

---

## Справочник по интерфейсу CLI

Глобальные соглашения: `-s/--slot <0|a|1|b|…|all>` выбирает копию LP-метаданных (по умолчанию `all` — все валидные слоты, как `lpdump -a`); `--suffix <a|b|all>` (только длинный флаг) фильтрует буквы имен внутри нее. Резолв базовых имен: `-p system --suffix a` находит `system_a`; беслотовые Virtual A/B разделы подходят всегда. Все команды принимают raw-образы, Android-sparse и raw-блочные устройства (`/dev/block/by-name/super`). `--device` (повторяемый, в `info`/`extract`/`read`/`add`/`resize`/`create`) привязывает retrofit-вторички как `name=path` либо автоматчится по имени (`vendor.img`, `super_vendor.img`, `*_vendor.img`). Размеры понимают `K`/`M`/`G` (любой регистр), в `resize` еще и `s` (секторы). Коды выхода: `0` успех, `1` ошибка, `2` неверный `--get`-ключ (info). Каждая подкоманда полностью документирована через `super-image-worker <команда> --help`.

### 1. `info` — инспектор (только чтение, root не нужен)

```bash
super-image-worker info super.img                              # Всё, человеческие таблицы
super-image-worker info super.img --suffix a -f tsv -H -c name,size_bytes | awk '{print $1}'
super-image-worker info /dev/block/by-name/super -s 1          # Метаслот 1 живого раздела
super-image-worker info super.img -f json | jq '.partitions[] | .name'
eval $(super-image-worker info super.img -f env); echo $SUPER_PART_SYSTEM_A_SIZE
super-image-worker info super.img --get partitions.system_a.size
super-image-worker info super.img --get partitions.system_a.extent.0.phys_offset_hex
super-image-worker info super.img --get available_suffixes
super-image-worker info sys.img --device vendor=vend.img       # Retrofit-пара + отчет привязок
```

Форматы: `human` (по умолчанию), `json` (pretty, безопасен для jq), `tsv` (`-H` убирает шапку, `-c` выбирает из 17 колонок), `env` (`SUPER_*`). Флаги секций `-i/-p/-e/-g/-d/-m/-a`, `-b` для сырых байтов в TSV, `--list-keys` показывает каталог всех `--get`-путей.

### 2. `extract` — аналог lpunpack (только чтение, root не нужен)

```bash
super-image-worker extract super.img -o ./out                  # Распаковать всё (параллельно на rayon)
super-image-worker extract super.img -o ./out -p system_a
super-image-worker extract super.img -o ./out --suffix a
super-image-worker extract sys.img -o ./out --device vendor=vend.img
super-image-worker extract super.img --dry-run                 # Только имена + размеры
super-image-worker extract super.img -o ./out --force          # Перезаписать выходы
```

Создает `<partition_name>.img` на раздел (пустые файлы для разделов без экстентов), существующие файлы пропускает без `--force`, неудачные выходы удаляет, но код выхода при любой неудаче все равно `1`.

### 3. `read` — потоковый stdout (только чтение, root не нужен)

```bash
super-image-worker read super.img -p system --suffix a | file -   # (-s = размер, --slot/--suffix только длинные)
super-image-worker read super.img -p vendor_a | sha256sum
super-image-worker read super.img -p vendor_a --skip 1M --size 10M | xxd | head   # BrokenPipe-safe
super-image-worker read sys.img -p vendor_a --device vendor=vend.img > vendor.img
```

Идет по всей мультиэкстентной/мультиблочной цепочке с RAM $O(1)$; диапазоны клампятся концом раздела.

### 4. `make` — аналог lpmake (пишет raw-образы, root не нужен)

```bash
super-image-worker make -o super.img --device super:4G --group default:4G \
    --partition system_a:readonly:default:system.img
super-image-worker make -o out/super --retrofit --device system:2G --device vendor:1G \
    --group google_dynamic_partitions_a:3G \
    --partition system_a:readonly:google_dynamic_partitions_a:sys.img
super-image-worker make -o super.img --sparse --slot all --device super:4G \
    --group g:4G --partition system_a:readonly:g:a.img --partition system_b:readonly:g:b.img
super-image-worker make -o super.img --device super:4G --group g:4G \
    --partition sys:readonly:g:sys.img --dry-run
super-image-worker make -o /dev/block/by-name/super --device super:9126805504 \
    --group g:9124708352 --partition system_a:none:g:system_a.img   # Сразу в блочное устройство
super-image-worker make -o s/super --retrofit --device super:8G --device cust:2G \
    --output-map super=/dev/block/by-name/super --output-map cust=/dev/block/by-name/cust \
    --group g:9G --partition vendor_a:none:g@cust:vendor_a.img      # Сплит в блоки, с пином
```

Спеки: `--device name:size[:alignment[:alignment_offset]]`, `--group name:max_size`, `--partition name:attrs:group[:payload]` (`readonly,slot_suffixed,updated,disabled,none`; без payload — extent-less заглушка), `--partition name:attrs:group@device:payload` пинит раздел на одно split-устройство. Слоты `0|1|a|b|all` (по умолчанию `0`, `a`/`b` — алиасы `0`/`1`); метаданные (геометрия + слоты) живут в файле первого устройства, вторички несут только данные; негабаритные payload режутся по устройствам; `--sparse` дает контейнеры из RAW + DONT_CARE (стейджинг `*.raw-tmp` удаляется); каждая сборка самопроверяется перезагрузкой. Выходы (`-o`, `--output-map name=path`) могут быть обычными файлами или raw-блочными устройствами: для блоков размер спеки обязан точно совпасть с probed-размером блока (проверяется и в `--dry-run`), узел зануляется перед записью, а `--sparse` в блок запрещен.

### 5. `add` — добавить раздел + payload (только raw, root не нужен)

```bash
super-image-worker add super.img -n my_part_a -p payload.img
super-image-worker add super.img -n custom -p data.bin -g my_group
super-image-worker add super.img -n test_a -p file.txt --attrs readonly,slot_suffixed
super-image-worker add super.img -n big_a -p big.img --force    # Поверх лимита maximum_size
super-image-worker add sys.img -n extra_a -p extra.img --device vendor=vend.img
```

Автовыбор группы (`qti_* > google_* > samsung_*/sec_* > mtk_* > largest`) с проверкой `maximum_size`; аллокация только на привязанных устройствах.

### 6. `resize` — безопасный ресайз (только raw, root не нужен)

```bash
super-image-worker resize super.img system_a 2G
super-image-worker resize super.img system --suffix a 900M
super-image-worker resize super.img vendor_a 512M --allow-shrink
super-image-worker resize super.img system_a 4G --force
super-image-worker resize super.img system_a 1G --dry-run
```

Рост расширяется на месте либо вшивает новый экстент; ужатие усекает/удаляет хвостовые экстенты (0 = удалить все).

### 7. `remove` — удалить раздел или группу (только raw, root не нужен)

```bash
super-image-worker remove super.img my_partition
super-image-worker remove super.img system --suffix a
super-image-worker remove super.img my_group --group
super-image-worker remove super.img my_group --group --force   # Каскад: разделы + экстенты
super-image-worker remove super.img test_a --dry-run
```

### 8. `rename` — переименовать запись (только raw, root не нужен)

```bash
super-image-worker rename super.img old_name new_name
super-image-worker rename super.img old_group new_group --group
super-image-worker rename super.img test_a test_b --dry-run
```

Только поле имени (≤36 байт, должно оставаться уникальным); checksums обновляются. `--slot` выбирает копию метаданных, если имя есть в нескольких слотах.

### 9. `create` — раздел только в метаданных (только raw, root не нужен)

```bash
super-image-worker create super.img -n new_part -g qti_dynamic_partitions_a
super-image-worker create super.img -n new_part -g new_group --group-size 4G --size 1M
super-image-worker create super.img -n empty_b -g default --size 0
super-image-worker create super.img -n big_a -g qti_dynamic_partitions_a --size 2G --force
```

Как `add`, но без байтов payload; `--size 0` резервирует extent-less заглушку.

### 10. `connect` / `disconnect` — loop-устройства (Linux + Android, root)

```bash
sudo super-image-worker connect super.img -p odm_a        # Печатает /dev/loopN (на Android /dev/block/loopN)
sudo super-image-worker connect super.img -p system --suffix a
sudo mount /dev/loop14 /mnt && sudo umount /mnt
sudo super-image-worker disconnect super.img -p odm_a
```

Только разделы из одного linear-экстента (фрагментированные отклоняются с подсказкой про `map`).

### 11. `map` / `unmap` — device-mapper (только Android/Recovery, root)

```bash
super-image-worker map super.img -p system_a              # -> /dev/block/mapper/system_a
super-image-worker map super.img -p vendor --suffix a --force-writable
super-image-worker unmap system_a
```

Полные цепочки экстентов (linear + zero). На хостовом Linux отказывается работать — там используйте `connect`.

### 12. `cow` — удаление OTA COW-разделов (только raw, root не нужен)

```bash
super-image-worker cow super.img                          # Удалить *-cow группы `cow` во всех слотах
super-image-worker cow /dev/block/by-name/super -s 0
super-image-worker cow super.img --dry-run
```

Аналог `lptools --clear-cow`. Отказывается работать, пока под `/dev/block/mapper` замаплен хоть один `-cow`-девайс (опасность живого мержа), обход через `--force`; exit 0, даже если удалять нечего.

### 13. `snapshot-status` — состояние OTA-обновления (только чтение, root не нужен)

```bash
super-image-worker snapshot-status                        # -> Update state: none
super-image-worker snapshot-status | grep '^Update state:'
```

Аналог `snapshotctl dump` для ветвлений инсталлера (`none`/`initiated`/`unverified`/`merging`/`merge-completed`/`merge-needs-reboot`/`merge-failed`/`cancelled`): читает `/metadata/ota/state` напрямую (поле #1 proto, fallback на legacy-текст, отсутствие/мусор → `none`). Одна строка в stdout, всегда exit 0.

---

## Важные нюансы окружения: переполнение `TMPDIR`

### Суть проблемы

Копирование 9-гигабайтного `super.img` (или извлечение всех разделов) в маленький `/tmp` (tmpfs, часто 8–16 ГБ) бесшумно забивает файловую систему и роняет сессию терминала посреди записи.

### Решение

Никогда не складывайте образы под `/tmp`. Используйте каталог на большом физическом томе и экспортируйте его на сессию:

```bash
# Разовое выполнение с явным рабочим каталогом:
mkdir -p ~/ws/tmp && cp /images/super.row.img ~/ws/tmp/work.img

# Экспорт на всю сессию терминала или внутрь shell-скрипта:
export TMPDIR=~/ws/tmp
```

Пути данных `read`/`extract`/`make` потоковые с RAM $O(1)$, но *выходные файлы и копии образов* все равно требуют реального места — закладывайте запас ~2× размера образа.

---

## Замеры производительности и валидация

Замерено на реальном 9-гигабайтном `super` (14 разделов, 7 с данными):

* `extract` (rayon, 7 разделов с данными): ~11 с из raw, ~34 с из Android Sparse (доминируют сики по чанкам).
* Raw-vs-sparse извлечение: все 7 разделов побитово идентичны (sha256).
* `read --size 100M` против дампа `extract`: sha256 совпали; окна `--skip/--size` совпадают с окнами `dd`.
* `make` одиночный 512 МиБ (payload 40+20+30 МиБ): метаданные только в запрошенном слоте; все извлечения совпадают с источниками; `--sparse` жмет 512 МиБ → 91 МиБ и читается идентично.
* `make --retrofit` 100 + 200 МиБ с payload 60 + 40 МиБ: перелив на устройство 1; сплит-`extract`/`read` совпадают с источниками.
* Payload 90 МиБ на устройствах 60 + 60 МиБ: автоматический спан из 2 экстентов (59 + 31 МиБ), round-trip цел.
* Изоляция слотов: после `add` primary+backup слота 0 меняются идентично, хеши слотов 1–2 нетронуты.
* Двухслотовый образ Pixel (текущий слот `_b`): `--slot 1 --suffix b` открывает живую таблицу; чтения блока побитово совпадают с файловым дампом (sha256).
* Сплит super по блочным узлам `super`+`cust`+`modem_a`+`modem_b` (`--output-map`, пины `@device`): сборки в файлы, `dd` в блоки и сборки сразу в блоки читаются одинаково под `lpdump` и `siw`.
* Legacy однослотовый образ (бессуффиксные `system`/`vendor`, один метаслот): round-trip генерация/чтение/extract/resize/rename проверен.
* `cow` на живом блоке `super`: no-op на чистой таблице, отказ при замапленном тестовом `-cow`, реальное удаление с кросс-чеком `lpdump`, блок восстановлен с совпадающим sha256.
* `snapshot-status`: вывод один в один с `snapshotupdater_static dump` плюс crafted proto/legacy/empty/missing-кейсы.
* `cargo clippy --all-targets -- -D warnings`: чисто. `cargo test`: pass.

---

## Лицензирование

Двойная лицензия на условиях [MIT License](LICENSE-MIT) и [Apache License 2.0](LICENSE-APACHE), на ваш выбор — та же схема, что и в sibling-проекте `image-worker`.
