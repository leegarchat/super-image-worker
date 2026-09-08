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
* **Встроенный аналог `lpmake` (`make`)**: генерация образов с нуля с произвольным целевым слотом (`0|1|a|b|all` — штатный `lpmake` пишет только слот 0), одиночным или retrofit-выводом, raw- или Sparse-контейнерами и мультиблочными spanning-экстентами, когда на одном устройстве места не хватает.
* **Многопоточное извлечение**: параллельный `extract` на `rayon`, у каждого потока изолированный дескриптор образа (гонок `Seek` нет).
* **CLI для скриптов**: `--slot` во всех командах, форматы `human`/`json`/`tsv`/`env`, извлечение скаляров через `--get` без завершающего перевода строки, потоковый `read` с `--skip`/`--size` и корректной обработкой `BrokenPipe`.

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

# Быстрая локальная сборка
cargo build --release
# -> target/release/super-image-worker (~1.2 МБ, LTO + stripped)

# Статические мультиархитектурные бинарники в dist/
./build.sh --cargo --arch x64      # x86_64-unknown-linux-musl
./build.sh --arch all              # x86_64, x86, aarch64, armv7

# Проверки
cargo clippy --all-targets -- -D warnings
cargo test
```

---

## Справочник по интерфейсу CLI

Глобальные соглашения: `--slot <a|b|all>` фильтрует по суффиксу разделов (плюс резолв базовых имен: `-p system -s a` находит `system_a`; беслотовые Virtual A/B разделы подходят всегда). `--device` (повторяемый, в `info`/`extract`/`read`/`add`/`resize`/`create`) привязывает retrofit-вторички как `name=path` либо автоматчится по имени (`vendor.img`, `super_vendor.img`, `*_vendor.img`). Размеры понимают `K`/`M`/`G` (любой регистр), в `resize` еще и `s` (секторы). Коды выхода: `0` успех, `1` ошибка, `2` неверный `--get`-ключ (info).

### 1. `info` — инспектор (только чтение, root не нужен)

```bash
super-image-worker info super.img                              # Всё, человеческие таблицы
super-image-worker info super.img -s a -f tsv -H -c name,size_bytes | awk '{print $1}'
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
super-image-worker extract super.img -o ./out -s a
super-image-worker extract sys.img -o ./out --device vendor=vend.img
super-image-worker extract super.img --dry-run                 # Только имена + размеры
super-image-worker extract super.img -o ./out --force          # Перезаписать выходы
```

Создает `<partition_name>.img` на раздел (пустые файлы для разделов без экстентов), существующие файлы пропускает без `--force`, неудачные выходы удаляет, но код выхода при любой неудаче все равно `1`.

### 3. `read` — потоковый stdout (только чтение, root не нужен)

```bash
super-image-worker read super.img -p system -S a | file -      # (-S = слот; -s = размер)
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
```

Спеки: `--device name:size[:alignment[:alignment_offset]]`, `--group name:max_size`, `--partition name:attrs:group[:payload]` (`readonly,slot_suffixed,updated,disabled,none`; без payload — extent-less заглушка). Слоты `0|1|a|b|all` (по умолчанию `0`); метаданные (геометрия + слоты) живут в файле первого устройства, вторички несут только данные; негабаритные payload режутся по устройствам; `--sparse` дает контейнеры из RAW + DONT_CARE (стейджинг `*.raw-tmp` удаляется); каждая сборка самопроверяется перезагрузкой.

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
super-image-worker resize super.img system -s a 900M
super-image-worker resize super.img vendor_a 512M --allow-shrink
super-image-worker resize super.img system_a 4G --force
super-image-worker resize super.img system_a 1G --dry-run
```

Рост расширяется на месте либо вшивает новый экстент; ужатие усекает/удаляет хвостовые экстенты (0 = удалить все).

### 7. `remove` — удалить раздел или группу (только raw, root не нужен)

```bash
super-image-worker remove super.img my_partition
super-image-worker remove super.img system -s a
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

Только поле имени (≤36 байт, должно оставаться уникальным); checksums обновляются.

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
sudo super-image-worker connect super.img -p odm_a        # Печатает /dev/loopN
sudo super-image-worker connect super.img -p system -s a
sudo mount /dev/loop14 /mnt && sudo umount /mnt
sudo super-image-worker disconnect super.img -p odm_a
```

Только разделы из одного linear-экстента (фрагментированные отклоняются с подсказкой про `map`).

### 11. `map` / `unmap` — device-mapper (только Android/Recovery, root)

```bash
super-image-worker map super.img -p system_a              # -> /dev/block/mapper/system_a
super-image-worker map super.img -p vendor -s a --force-writable
super-image-worker unmap system_a
```

Полные цепочки экстентов (linear + zero). На хостовом Linux отказывается работать — там используйте `connect`.

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
* `cargo clippy --all-targets -- -D warnings`: чисто. `cargo test`: pass.

---

## Лицензирование

Двойная лицензия на условиях [MIT License](LICENSE-MIT) и [Apache License 2.0](LICENSE-APACHE), на ваш выбор — та же схема, что и в sibling-проекте `image-worker`.
