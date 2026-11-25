# LrCatFix — nástroj pro dohledání a opravu chybějících fotek v Lightroom Classic katalogu

> Specifikace pro implementaci. Cílová platforma: **Windows 10/11 x64**. Sekundárně Linux (bez MFT indexu).

---

## 1. Účel

Lightroom Classic drží cesty k souborům v katalogu `*.lrcat` (SQLite). Když se soubory přesunou mimo LrC, katalog má "!" u fotek a vestavěná funkce *Find Missing Photos* umí opravit jen složku po složce a jen ručně.

Nástroj má:

1. Načíst `.lrcat`, vytáhnout všech ~30 000 záznamů souborů a jejich očekávané absolutní cesty.
2. Ověřit existenci každého souboru na disku (rychle, dávkově).
3. Pro chybějící soubory postavit index disku ve stylu *Everything* a najít kandidáty podle jména.
4. Nabídnout uživateli rozhodnutí (auto / dialog při nejednoznačnosti / přeskočit / zrušit).
5. Detekovat **rozsypané složky** — situaci, kdy fotky z jednoho adresáře skončily rozházené po mnoha složkách, nebo naopak celá složka se přesunula jinam — a nabídnout hromadné řešení: buď **přepis cesty v DB (repoint)**, nebo **fyzické sloučení složek (consolidate)**.
6. Vše zapisovat transakčně, se zálohou katalogu a reverzibilním journalem.

**Nefunkční cíl:** nikdy nesmí dojít k tichému poškození katalogu. Raději operaci odmítnout než provést nejistý zápis.

---

## 2. Kritické předpoklady a bezpečnostní pravidla

Tato sekce má přednost před vším ostatním. Implementuj ji jako první, ne poslední.

| Pravidlo | Implementace |
|---|---|
| LrC musí být zavřený | Detekovat `<katalog>.lrcat.lock` a `<katalog>.lrcat-journal` / `-wal`. Pokud existují → **odmítnout** otevření pro zápis, nabídnout read-only režim. |
| Záloha před prvním zápisem | Kopie `*.lrcat` + `*.lrcat-data` (LrC 11+) do `<katalog>/LrCatFix-backups/YYYYMMDD-HHMMSS/`. Ověřit velikost a `PRAGMA integrity_check` na kopii. Bez úspěšné zálohy se nezapisuje. |
| Nikdy neměnit `journal_mode` | LrC očekává svůj režim. Otevřít připojení, nesahat na `journal_mode`, `page_size`, `encoding`, `user_version`. Zákaz `VACUUM`, zákaz `writable_schema`. |
| Schema guard | Před jakoukoli operací načíst `sqlite_master`, ověřit existenci tabulek a **konkrétních sloupců** (viz §3). Při neshodě → chyba s výpisem, co chybí. Podporované verze se ověřují empiricky, ne odhadem. |
| Vše v transakci | Jeden `BEGIN IMMEDIATE` … `COMMIT` na dávku. Při jakékoli chybě `ROLLBACK`. |
| Dry-run default | Aplikace startuje v režimu **náhled**. Zápis vyžaduje explicitní přepnutí + potvrzovací dialog se souhrnem (N řádků DB, N přesunů souborů). |
| Journal změn | Každá provedená operace se zapíše do `journal.jsonl` **před** commitem i po něm (fáze `planned` → `done`). Undo čte journal pozpátku. |
| Přesuny souborů | Preferovat `MoveFileEx` v rámci svazku (atomické rename). Přes svazky: copy → verify (velikost + hash) → delete. Nikdy `delete` před ověřeným `copy`. |
| Pre-flight kontrola místa | Před zálohou zjisti velikost `*.lrcat` + `*.lrcat-data` a volné místo na cílovém svazku (`GetDiskFreeSpaceEx`). Vyžaduj **2× velikost + 500 MB rezervy**. Nedostatek místa → odmítnout a nabídnout jiný cíl zálohy. Nikdy nezačínat kopírování „a uvidíme". |
| Nedokončená záloha | Kopíruj do `*.tmp`, po ověření velikosti a `integrity_check` přejmenuj. Poškozená/částečná záloha nesmí nikdy vypadat jako platná. |
| Cloudové a offline soubory | Před hashováním a čtením EXIF kontroluj atributy souboru — viz §5.1.1. Nikdy nespouštěj stahování z cloudu bez vědomí uživatele. |

---

## 3. Datový model `.lrcat`

`.lrcat` je běžná SQLite 3 databáze. Relevantní část schématu (ověř za běhu, sloupce se mezi verzemi LrC mírně liší):

```
AgLibraryRootFolder(id_local, id_global, absolutePath, name, relativePathFromCatalog)
AgLibraryFolder(id_local, id_global, pathFromRoot, rootFolder → AgLibraryRootFolder.id_local, visibility)
AgLibraryFile(id_local, id_global, folder → AgLibraryFolder.id_local,
              baseName, extension, idx_filename, lc_idx_filename,
              originalFilename, importHash, md5, modTime, externalModTime,
              fileFormat, sidecarExtensions, errorMessage)
Adobe_images(id_local, id_global, rootFile → AgLibraryFile.id_local, captureTime, fileFormat, ...)
AgLibraryCollection(id_local, name, ...)
AgLibraryCollectionImage(collection, image → Adobe_images.id_local)
```

### 3.1 Sestavení očekávané cesty

```sql
SELECT
    fi.id_local              AS file_id,
    rf.id_local              AS root_id,
    rf.absolutePath          AS root_path,
    fo.id_local              AS folder_id,
    fo.pathFromRoot          AS rel_path,
    fi.idx_filename          AS filename,
    fi.baseName, fi.extension,
    fi.externalModTime, fi.md5
FROM AgLibraryFile fi
JOIN AgLibraryFolder     fo ON fi.folder     = fo.id_local
JOIN AgLibraryRootFolder rf ON fo.rootFolder = rf.id_local;
```

Normalizace:

* `absolutePath` je ve tvaru `C:/Foto/` — dopředná lomítka, **koncové lomítko**.
* `pathFromRoot` je `2019/2019-07 Chorvatsko/` nebo prázdný řetězec pro kořen, taktéž s koncovým lomítkem.
* Výsledek: `absolutePath + pathFromRoot + idx_filename`, převést na `\` na Windows.
* Porovnání cest a jmen na Windows **case-insensitive** (`lc_idx_filename` je lowercase varianta, používaná LrC k indexaci — při zápisu ji musíš udržovat konzistentní).
* **Katalog z macOS:** `absolutePath` může být POSIX cesta (`/Users/pavel/Pictures/`) nebo `/Volumes/Disk/`. V takovém případě neexistuje *žádná* mapovatelná cesta a fáze VERIFY označí 100 % souborů jako chybějící. Detekuj to explicitně (`absolutePath` nezačíná `X:/` ani `//`) a místo záplavy chybějících souborů zobraz dialog **„Katalog pochází z jiného systému — přiřaď kořenové složky"**, kde uživatel ručně mapuje každý `AgLibraryRootFolder` na lokální cestu. Teprve pak pokračuj. Totéž platí opačně (katalog z Windows otevřený na Linuxu).
* Interně pracuj s jedním normalizovaným tvarem (dopředná lomítka, bez koncového lomítka, `NFC` unicode normalizace kvůli diakritice z macOS) a na nativní tvar převáděj až v okamžiku I/O.

### 3.2 Rozšiřující dotazy (pro UI kontext)

* Kolik fotek v katalogu má daná složka: `GROUP BY fo.id_local`.
* Do kterých kolekcí/alb fotka patří — přes `Adobe_images.rootFile` → `AgLibraryCollectionImage.image`. Používá se k detekci "album rozsypané po složkách" (§7.3).

**Virtuální kopie:** `Adobe_images` obsahuje i virtuální kopie, které **nemají vlastní fyzický soubor** — několik řádků `Adobe_images` sdílí jeden `rootFile`. Důsledky:

* Fáze EXTRACT/VERIFY pracuje výhradně nad `AgLibraryFile` (§3.1), takže je proti tomuto problému imunní — každý fyzický soubor je tam právě jednou. **Nikdy nezakládej hlavní pipeline na `Adobe_images`.**
* Kdekoli joinuješ `Adobe_images` (kolekce, §7.3), počítej s duplicitami: agreguj přes `rootFile`, ne přes `Adobe_images.id_local`.
* Sloupec rozlišující master vs. virtuální kopii (pravděpodobně `masterImage` a `copyName`) **ověř empiricky** dotazem na reálný katalog: `SELECT count(*), copyName IS NOT NULL FROM Adobe_images GROUP BY 2;`. Nepiš kód podle domněnky.
* V UI zobrazuj počty jako „X souborů (Y fotek včetně virtuálních kopií)", aby se čísla nerozcházela s tím, co ukazuje LrC.

### 3.3 Zápis — co se mění

**Případ A: soubor nalezen ve složce, kterou katalog už zná**

```sql
UPDATE AgLibraryFile SET folder = :new_folder_id WHERE id_local = :file_id;
```

**Případ B: nalezen v neznámé složce** → nejdřív vytvořit `AgLibraryFolder` (a případně `AgLibraryRootFolder`), pak jako A.

**Případ C: liší se i jméno souboru** (přejmenování) → navíc:

```sql
UPDATE AgLibraryFile
   SET baseName = :base, extension = :ext,
       idx_filename = :name, lc_idx_filename = lower(:name)
 WHERE id_local = :file_id;
```

`originalFilename` **neměnit** (je to jméno při importu). `md5` a `externalModTime` volitelně obnovit — chovej se k tomu jako k opt-in přepínači, default nechat beze změny.

### 3.4 Generování `id_local` a `id_global`

* `id_local`: `SELECT MAX(id_local)+1 FROM <tabulka>` v rámci téže transakce. **Navíc** ověř, jestli katalog nedrží globální čítač v `Adobe_variablesTable` (`SELECT name, value FROM Adobe_variablesTable WHERE name LIKE '%ounter%'`) — pokud ano, inkrementuj i ten. Toto ověř na reálném katalogu při implementaci, nespoléhej na tento dokument.
* `id_global`: LrC používá 32 znaků velkých hex bez pomlček (UUIDv4 bez dashů, uppercase). Generuj stejně a ověř unikátnost dotazem.

### 3.5 Sidecary a RAW+JPEG páry

Fotka v LrC skoro nikdy není jeden soubor. Přesun, který zapomene na doprovodné soubory, je **nevratná ztráta úprav** — a undo ji nezachrání, protože o těch souborech nástroj neví.

**Zdroj pravdy:** sloupec `AgLibraryFile.sidecarExtensions` — LrC v něm drží čárkou oddělený seznam přípon, které k souboru patří (typicky `xmp`, u některých importů i `jpg`). Čti ho, nedomýšlej se.

**Nad rámec toho hledej v cílové složce soubory se shodným `baseName` a příponami:**

| Přípona | Co to je | Riziko při ztrátě |
|---|---|---|
| `.xmp` | vývojová nastavení pro RAW | ztráta všech úprav |
| `.thm` | náhled k videu | kosmetické |
| `.wav` | hlasová poznámka k fotce | ztráta dat |
| `.aae` | úpravy z iOS | ztráta úprav |
| `.dop`, `.pp3`, `.arp` | sidecary jiných vývojek | ztráta úprav |

**RAW + JPEG:** LrC má volbu *„Treat JPEG files next to raw files as separate photos"*. Podle jejího stavu při importu:

* **vypnuto** → JPEG je sidecar RAWu, v `AgLibraryFile` je **jeden** řádek;
* **zapnuto** → jsou to **dva nezávislé** řádky se stejným `baseName` a různým `extension`.

Nástroj musí zvládnout obojí, a hlavně: **nesmí předpokládat, že shodný `baseName` znamená jeden objekt.** Před přesunem zjisti, jestli druhý soubor páru má v katalogu vlastní `id_local`. Pokud ano, plánuj ho jako samostatnou položku (může mít jiný cíl!). Pokud ne, veze se jako sidecar.

**Pravidla přesunu:**

1. Přesun je **atomická skupina**: hlavní soubor + všechny jeho sidecary. Selže-li kterýkoli člen, vrať zpět celou skupinu.
2. Sidecary se do journalu zapisují jako samostatné `file_move` záznamy, ale se společným `group_id`.
3. Když sidecar v cíli už existuje a liší se obsahem → **nikdy nepřepisuj**, eskaluj na uživatele.
4. Ve fázi MATCH: nalezený kandidát bez svého `.xmp`, zatímco původní očekávaná složka `.xmp` má, je varovný signál — zobraz ho v UI, ale sám nerozhoduj.

---

## 4. Volba jazyka a stacku

### Rozhodnutí: **Rust**

| Kritérium | Rust | C# / .NET 8 | Python |
|---|---|---|---|
| SQLite | `rusqlite` (bundled, statické) | `Microsoft.Data.Sqlite` | `sqlite3` (stdlib) |
| Paralelní walk / hash | `rayon`, `jwalk`, `blake3` — plný multicore | dobrý (TPL) | GIL, nutné procesy |
| Čtení NTFS MFT (Everything-like) | `windows` crate, přímý `DeviceIoControl` | P/Invoke, snadné | ctypes, křehké |
| Distribuce | jeden `.exe`, bez runtime | vyžaduje runtime nebo AOT publish | balení je peklo |
| GUI | `eframe`/`egui` — immediate mode, virtualizované tabulky na 100k+ řádků | WPF/WinUI hezčí, ale více kódu | PySide6 fajn, ale +100 MB |
| Vhodnost pro agentní vývoj | typový systém chytá chyby v compile loop | ok | runtime chyby až za běhu |

**Stack:**

```toml
eframe / egui          # GUI, immediate mode
egui_extras            # TableBuilder (virtualizované tabulky), obrázky
rusqlite               # feature = ["bundled", "backup"]
rayon                  # paralelní stat/hash
jwalk                  # paralelní rekurzivní walk (fallback index)
windows                # DeviceIoControl, FSCTL_ENUM_USN_DATA, MoveFileEx
serde / serde_json     # journal, config
blake3 + md-5          # hash verifikace (md5 kvůli kompatibilitě se sloupcem md5)
chrono                 # timestampy
anyhow / thiserror     # chyby
tracing                # logování
kamadak-exif           # EXIF čtení - vestavěný JPEG náhled pro dialog duplikátů (§6.2, přidáno M7)
image (jen "jpeg")     # dekódování EXIF náhledu do pixelů pro zobrazení (přidáno M7)
```

**Alternativa, pokud chceš rychlejší UI iteraci:** C# / .NET 8 + WPF (nebo Avalonia pro multiplatformu). Výkonově naprosto stačí, ekosystém pro Windows API je pohodlnější. Nezvolil jsem ji kvůli runtime závislosti a horší kontrole nad paralelním I/O.

**Python nedoporučuji** jako primární — ne kvůli 30k řádkům (to je triviální), ale kvůli indexu disku (miliony souborů), MFT přístupu a balení.

---

## 5. Architektura — pipeline

```
[0] OPEN      → validace zámků, schema guard, záloha
[1] EXTRACT   → SQL join → Vec<CatalogFile> (~30k)
[2] VERIFY    → existence check → missing: Vec<CatalogFile>
[3] INDEX     → Everything IPC | MFT scan | walk → NameIndex
[4] MATCH     → kandidáti + skóre → Vec<Resolution>
[5] CLUSTER   → detekce vzorů složek → Vec<FolderPlan>
[6] REVIEW    → UI: uživatel schvaluje/upravuje plán
[7] APPLY     → transakce DB + přesuny souborů + journal
[8] UNDO      → přehrání journalu pozpátku
```

Fáze 1–5 jsou čistě read-only a spustitelné opakovaně. Stav mezi fázemi je serializovatelný do `session.json` (aby se dala práce přerušit a vrátit se k ní).

### 5.1 Fáze VERIFY — výkon

Naivně 30 000× `stat()` = pomalé na síťovém nebo pomalém disku. Místo toho:

1. Seskup soubory podle `folder_id` (typicky ~1 000–3 000 unikátních složek).
2. Pro každou složku jeden `read_dir`, výsledek do `HashSet<String>` (lowercase jména).
3. Porovnej množinově. Neexistující složka → všechny její soubory chybí (a označ složku jako `FolderMissing`, což je silný signál pro §7).
4. Paralelně přes `rayon`, ale s omezeným počtem vláken na síťové svazky (konfigurovatelné, default 4 pro UNC, 16 pro lokální).

Cíl: < 5 s pro 30k souborů na lokálním SSD.

#### 5.1.1 Cloudové a offline soubory (OneDrive, Google Drive, iCloud, Dropbox)

Na Windows 10/11 může soubor fyzicky existovat jen jako placeholder. Metadata (jméno, velikost, čas) jsou lokální — **existence check je tedy bezpečný a rychlý**. Zabijákem je až čtení obsahu: MD5 nebo EXIF náhled spustí stahování, vlákno se zablokuje na desítky sekund a při 30k souborech to znamená stažení stovek GB.

**Detekce** přes `GetFileAttributesW` / `FILE_ATTRIBUTE_TAG_INFO`:

```
FILE_ATTRIBUTE_OFFLINE                  (0x1000)   klasický HSM
FILE_ATTRIBUTE_RECALL_ON_OPEN           (0x40000)  placeholder
FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS    (0x400000) OneDrive "Files On-Demand"
FILE_ATTRIBUTE_REPARSE_POINT            (0x400)    + ověřit reparse tag
```

**Pravidla:**

1. Soubor s kterýmkoli z těchto atributů označ jako `CloudPlaceholder`.
2. `use_md5` a EXIF náhled se na ně **nikdy nespustí automaticky** — ani v režimu `always`. Skóre se počítá bez těchto signálů, ostatní signály (jméno, velikost, čas, sourozenci) fungují normálně.
3. V UI ikona ☁ ve sloupci Stav. Tlačítko „Stáhnout a ověřit (N souborů, ~X GB)" s explicitním potvrzením a odhadem objemu.
4. Při **přesunu** placeholderu: `MoveFileEx` v rámci téhož cloudového kořene je v pořádku. Přesun **ven** z cloudové složky vyvolá stažení — varuj předem s odhadem velikosti.
5. Otevírej soubory s flagem `FILE_FLAG_OPEN_NO_RECALL` tam, kde jde jen o metadata.
6. Stejné zacházení aplikuj na síťové svazky se zapnutým offline cachováním.

### 5.2 Fáze INDEX — "Everything" vrstva

Tři backendy, zkoušené v tomto pořadí:

**A) Everything IPC** (pokud běží) — nejrychlejší a bez admin práv.
* Detekce okna `EVERYTHING_TASKBAR_NOTIFICATION` / IPC přes `WM_COPYDATA`, nebo volání `Everything64.dll` SDK, nebo `es.exe` jako fallback subprocess.
* Dotaz na přesné jméno: `file:exact:"IMG_1234.CR2"`. Dávkovat — jeden dotaz na soubor je zbytečný, lze použít OR dotazy po ~50 jménech nebo prostě vytáhnout kandidáty podle přípony a filtrovat lokálně.

**B) Vlastní MFT scan** (NTFS, vyžaduje elevaci) — `DeviceIoControl(FSCTL_ENUM_USN_DATA)` nad každým NTFS svazkem, sestavit mapu `FRN → (parentFRN, name)`, z ní rekonstruovat cesty. Miliony záznamů za jednotky sekund, paměť ~150–300 MB.

> **Elevace je architektonické rozhodnutí, ne detail.** Otevření svazku přes `\\.\C:` vyžaduje `SE_MANAGE_VOLUME` / práva správce. Pravidla:
> * Aplikace se **nikdy nespouští elevovaná automaticky** a manifest nesmí obsahovat `requireAdministrator`. Běžný provoz (čtení katalogu, VERIFY, walk) elevaci nepotřebuje a spouštět nástroj, který zapisuje do katalogu, trvale jako správce je zbytečné riziko.
> * Při startu zjisti stav tokenu (`GetTokenInformation` / `TokenElevation`) a ulož ho do stavu aplikace. Backend B je dostupný jen když je token elevovaný.
> * Bez elevace: **žádný UAC prompt při startu**. V UI se u tlačítka „Indexovat" zobrazí volba „Rychlý sken disku (vyžaduje správce) 🛡" — teprve kliknutí spustí restart aplikace s `runas` a předá session přes `--resume <session.json>`, aby uživatel nepřišel o rozdělanou práci.
> * Selhání `DeviceIoControl` s `ERROR_ACCESS_DENIED` nebo `ERROR_INVALID_FUNCTION` (ne-NTFS svazek) → **tichý fallback na C)**, ne chybová hláška. Do logu zapiš důvod.
> * MFT scan přeskoč u svazků, které nejsou NTFS/ReFS (zjisti `GetVolumeInformation`), a u síťových svazků, kde principiálně nefunguje.

**C) Paralelní walk** (`jwalk`) nad uživatelem zvolenými kořeny — fallback pro ne-NTFS, síťové disky, Linux. Nabídnout výběr kořenů v UI (default: svazky, kde leží `AgLibraryRootFolder.absolutePath`).

Výstup je jednotný:

```rust
struct NameIndex {
    // lowercase filename → seznam absolutních cest
    by_name: HashMap<String, SmallVec<[PathBuf; 2]>>,
}
```

Index cachovat na disk (`index.bin`, bincode) s TTL a možností "Refresh index".

> **Implementace M7 (`src/index/mft.rs`, `src/fsops/shell.rs::relaunch_elevated`, `src/session.rs`):**
> * Backend B hotový: `unsafe` izolovaný v `index/mft.rs` (`win` submodul) - `CreateFileW`/`DeviceIoControl(FSCTL_ENUM_USN_DATA)`/`GetTokenInformation`. Selhání (jakýkoli `MftError`, nejen ty dva jmenované kódy) vede k tichému pádu na walk pro CELÉ volání (ne per-svazek mix backendu B a C) - jednodušší a stejně bezpečné.
> * Elevace: detekce při startu (`AppState::elevated`), tlačítko "Rychlý sken disku (vyžaduje správce) 🛡" v toolbaru (zobrazí se jen když `!elevated`), `ShellExecuteW` s verb `runas`. Session pro `--resume` (`src/session.rs`) ukládá VĚDOMĚ jen cestu ke katalogu - žádný jiný UI stav (filtry, výběr, rozpracované dialogy) se mezi restarty nepřenáší, protože zápis do katalogu vždy vyžaduje samostatné potvrzení, takže restart nikdy nepřerušuje nic nevratného.
> * Cache (`index::cache`) je teď sdílená mezi backendem B i C - cache soubor neví, kterým z nich vznikl (oba mají stejný tvar dat, plný index svazku).
> * **Výkonový cíl §10 ("index 2M souborů pod 15 s") NENÍ ověřený na reálném velkém NTFS svazku** - vývojové prostředí nemá elevovaný přístup k takovému svazku k dispozici. Testováno je jen: (a) korektnost parsování `USN_RECORD_V2` bufferu proti skutečnému layoutu structu (jednotkové testy), (b) reálné volání `CreateFileW`/`DeviceIoControl` nad `C:` v neelevovaném prostředí (očekávaně `AccessDenied`, ověřuje, že se to nezasekne/nespadne). Před spoléháním na tenhle výkonový cíl v produkci je potřeba manuální ověření na reálném stroji s administrátorským přístupem.

### 5.3 Fáze MATCH — skórování kandidátů

Pro každý chybějící soubor: kandidáti = `by_name[lowercase(filename)]`. Skóre 0–100:

| Signál | Body | Poznámka |
|---|---|---|
| Přesná shoda jména | vstupní podmínka | — |
| Velikost souboru odpovídá | +30 | pokud známa (viz níže) |
| `externalModTime` odpovídá (±2 s) | +25 | **epochu zkalibrovat empiricky** — viz §5.4 |
| Sourozenci: v téže cílové složce leží další hledané soubory ze stejné zdrojové složky | +25 | nejsilnější signál v praxi |
| Relativní podcesta souhlasí přesně (např. `2019/2019-07 Chorvatsko/`) | +15 | |
| Jméno cílové složky je **podobné** očekávanému | +8 | normalizovaná Levenshteinova vzdálenost ≥ 0,7 (crate `strsim`), po odstranění diakritiky a sjednocení oddělovačů `-_ ` |
| MD5 shoda se sloupcem `md5` | +100 (jistota) | počítá se jen na vyžádání / při remíze, **nikdy na cloud placeholderech** |
| Kandidát je uvnitř složky, kterou katalog už zná | +5 | |
| Kandidát leží v koši / `$RECYCLE.BIN` / `Temp` | −50 | |
| Kandidát leží v zálohách LrC (`*/Backups/*`) nebo v `*.lrdata` | −100 | fakticky vyloučení, viz §14 |
| Kandidát je cloud placeholder | −5 | slabá penalizace: je to platný soubor, ale nelze ho ověřit hashem |

Prahy: `≥70 a jediný kandidát v rozmezí 15 bodů od druhého` → **auto-resolve** (jen pokud je zapnuté "Auto"). Jinak → do fronty na ruční rozhodnutí.

### 5.4 Kalibrace času a velikosti

Katalog nemusí obsahovat velikost souboru přímo a formát `externalModTime` se mezi verzemi liší (Unix epocha vs. Apple epocha 2001-01-01). **Neháduj.** Postup:

1. Vezmi 200 souborů, které existují (z fáze VERIFY).
2. Porovnej jejich skutečný `mtime` s hodnotou v DB, urči offset a jednotku.
3. Pokud korelace < 95 %, signál "čas" úplně vypni a v UI to oznam.

**Tolerance porovnání — nikdy nepožaduj přesnost na milisekundy:**

| Situace | Tolerance |
|---|---|
| NTFS ↔ NTFS | ±2 s |
| exFAT / FAT32 (SD karty, externí disky) | **±2 s minimum** — tyto FS ukládají mtime s granularitou 2 s |
| FAT ukládá **lokální čas**, ne UTC | navíc akceptuj posun **přesně ±1 h** (letní/zimní čas) a celé hodiny při změně časové zóny |
| Kopírování mezi FS obecně | akceptuj posun o celé hodiny jako částečnou shodu (+12 místo +25) |

Prakticky: implementuj `time_score(a, b) -> u8`, který vrátí plné body při rozdílu ≤ 2 s, poloviční při rozdílu, který je celočíselným násobkem 3600 s (±2 s), a nula jinak. Typ FS zjisti přes `GetVolumeInformation` a toleranci nastav podle něj.

Stejně přistup ke sloupci `md5` — ověř na existujících souborech, jestli odpovídá MD5 celého souboru, nebo jen části. Pokud ne, nepoužívej ho.

---

## 6. UI — rozvržení

Styl **Total Commander**: dva panely, klávesnice napřed, žádné modální wizardy tam, kde stačí tabulka.

```
┌─ Toolbar ─────────────────────────────────────────────────────────────────┐
│ [Otevřít katalog] [Ověřit] [Indexovat] [Auto-match] │ ☐ Zápis povolen     │
│ [Použít změny] [Undo] │ Filtr: [____] [Jen nevyřešené ▾]                   │
├────────────────────────────┬──────────────────────────────────────────────┤
│ LEVÝ PANEL                 │ PRAVÝ PANEL                                  │
│ Strom složek katalogu      │ Soubory ve vybrané složce                    │
│                            │                                              │
│ ▾ C:/Foto        [412 ✗]   │ Jméno        Stav        Kandidát      Skóre │
│   ▾ 2019         [ 88 ✗]   │ IMG_001.CR2  ✗ chybí     D:/Zaloha/…    92  │
│     • 2019-07 HR [ 88 ✗]   │ IMG_002.CR2  ✗ chybí     3 kandidáti    —   │
│     • 2019-08    [  0 ]    │ IMG_003.CR2  ✓ ok        —              —   │
│   ▸ 2020         [324 ✗]   │ IMG_004.CR2  ✗ nenalezen —              —   │
│                            │                                              │
│ Legenda: ✗ chybí           │ [Vyřešit] [Přeskočit] [Hledat ručně…]        │
├────────────────────────────┴──────────────────────────────────────────────┤
│ NÁVRHY (cluster panel)                                                    │
│ ⚠ Složka "2019/2019-07 HR": 86 z 88 souborů nalezeno v D:/Zaloha/HR2019/  │
│   → [Přepsat cestu v katalogu]  [Přesunout soubory zpět]  [Detail…]       │
│ ⚠ Album "Nejlepší 2020": 44 fotek rozházeno ve 39 složkách                │
│   → [Sloučit do C:/Foto/2020/Nejlepsi/]  [Detail…]                        │
├───────────────────────────────────────────────────────────────────────────┤
│ Log / progress:  [████████░░] Indexuji D:  1 284 933 položek…             │
└───────────────────────────────────────────────────────────────────────────┘
```

### 6.1 Panely

* **Levý (strom):** hierarchie `AgLibraryRootFolder` → `AgLibraryFolder`. U každé složky badge s počtem chybějících / celkem. Barevné kódování: zelená (0 chybí), oranžová (část), červená (celá složka nenalezena).
* **Pravý (tabulka):** virtualizovaná (`egui_extras::TableBuilder`), sloupce: Jméno · Stav · Očekávaná cesta · Nalezená cesta · Skóre · Počet kandidátů · Akce. Řaditelné, filtrovatelné, multi-select (Shift/Ctrl, Space jako TC).
* **Dolní (návrhy):** výstup fáze CLUSTER. Každý návrh = jedna karta s akčními tlačítky.
* **Status bar:** progress, počty, režim (NÁHLED / ZÁPIS POVOLEN — vizuálně výrazně odlišené, zápisový režim červeně).

### 6.2 Dialog: výběr z duplikátů

Otevře se při ruční resolvi nebo když auto-match narazí na remízu.

```
┌ Vyber správný soubor: IMG_1234.CR2 ───────────────────────────────┐
│ Očekáváno: C:/Foto/2019/2019-07 HR/IMG_1234.CR2                   │
│                                                                    │
│  #  Cesta                              Velikost   Změněno    Skóre │
│  1  D:/Zaloha/HR2019/IMG_1234.CR2      24,1 MB   2019-07-14   92   │
│  2  E:/Old/dump/IMG_1234.CR2           24,1 MB   2021-03-02   61   │
│  3  D:/Zaloha/HR2019/kopie/IMG_1234.CR2 8,0 MB   2019-07-14   44   │
│                                                                    │
│  [Náhled]  [Porovnat MD5 všech]  [Otevřít v Průzkumníku]           │
│  ☐ Použít stejnou volbu pro zbytek souborů ze stejné složky        │
│                                                                    │
│  [Použít vybraný]  [Přeskočit]  [Přeskočit vše]  [Zrušit]          │
└────────────────────────────────────────────────────────────────────┘
```

* Náhled: vložený EXIF thumbnail (u RAW) nebo dekódovaný JPEG. Volitelné, ale u výběru z duplikátů to zásadně pomáhá.
* "Porovnat MD5" spočítá hash kandidátů na pozadí a doplní sloupec.
* Checkbox "použít pro zbytek složky" je nejdůležitější prvek — bez něj by uživatel odklikával stovky dialogů.

**Implementace M4, odchylky od téhle sekce (`src/app/dialogs/duplicates.rs`):**

* **Náhled (EXIF thumbnail) implementován až v M7** (`src/fsops/exif_thumbnail.rs`), po schválení `kamadak-exif` a `image` (JPEG dekódování) uživatelem - viz §4. M4 tohle vědomě vynechalo, protože žádný z těch crate v tu chvíli nebyl schválený. Čte jen vestavěný JPEG náhled z `JPEGInterchangeFormat`/`Length` (IFD1) - nedekóduje samotný RAW obraz.
* **"Použít stejnou volbu pro zbytek souborů ze stejné složky" implementováno jako:** vezmi rodičovskou složku vybraného kandidáta (`target_dir`); pro každý další dosud nevyřešený soubor ve STEJNÉ zdrojové složce, který má mezi svými MATCH kandidáty jednoho ležícího v `target_dir`, tohoto kandidáta rovnou zvol bez otevření dialogu. Soubory bez takového kandidáta zůstávají nevyřešené (nikdy se nehádá "nejbližší" kandidát). Zápis do DB (`catalog::apply::plan_manual_resolve` + `known_folders_by_path`) je oddělený krok za samostatným potvrzením ("Použít vyřešené (N)") — výběr kandidáta sám o sobě nic nezapisuje.
* **"Otevřít v Průzkumníku" (§6.3)** implementováno v `src/fsops/shell.rs` (`ShellExecuteW`, thin `unsafe` wrapper) a zapojeno do dialogu duplikátů - do kontextového menu tabulky (`Ctrl+Enter`) zatím ne, přijde spolu s klávesovými zkratkami (§6.4, M5+).

### 6.3 Dialog: sloučení / přepis složky

```
┌ Návrh: 2019/2019-07 HR ───────────────────────────────────────────┐
│ 86 z 88 souborů nalezeno v: D:/Zaloha/HR2019/                     │
│ 2 soubory nenalezeny nikde.                                        │
│                                                                    │
│ ○ Přepsat cestu v katalogu (žádné soubory se nehýbou)             │
│   → 1 UPDATE v AgLibraryFolder, 0 přesunů na disku. Doporučeno.   │
│                                                                    │
│ ○ Přesunout soubory zpět do C:/Foto/2019/2019-07 HR/              │
│   → 86 přesunů (2,1 GB), 0 změn v katalogu.                       │
│   ⚠ V cíli už existují 3 soubory se stejným jménem → [Řešit…]     │
│                                                                    │
│ [Zobrazit seznam všech 86]  [Otevřít zdroj]  [Otevřít cíl]        │
│ [Použít]  [Přeskočit]  [Zrušit]                                   │
└────────────────────────────────────────────────────────────────────┘
```

**„Otevřít v Průzkumníku"** patří do každého dialogu i do kontextového menu tabulky (`Ctrl+Enter`). Je to poslední instance, kde si uživatel může ověřit, že nástroj nelže, a bez ní lidé oprávněně odmítnou hromadné operace potvrdit. Implementace: `ShellExecuteW("open", "explorer.exe", "/select,\"<cesta>\"")` — varianta `/select` označí konkrétní soubor, ne jen otevře složku. Na Linuxu `xdg-open` nad rodičovským adresářem.

### 6.4 Klávesové zkratky

`F3` náhled · `Space` označit · `Enter` vyřešit · `F5` znovu ověřit · `Ctrl+Z` undo · `Ctrl+A` vybrat vše · `Del` označit jako "přeskočeno" · `Tab` přepnout panel.

---

## 7. Fáze CLUSTER — inteligentní detekce vzorů

Tohle je to, co odlišuje nástroj od "Find Missing Photos" v LrC. Vstup: výsledky MATCH. Výstup: seznam `FolderPlan`.

### 7.1 Vzor A — celá složka se přestěhovala (`FOLDER_MOVED`)

**Detekce:** pro zdrojovou složku `S` s `n` chybějícími soubory platí, že ≥ 80 % nalezených kandidátů (s nejvyšším skóre) leží v jedné cílové složce `T`, a `n ≥ 3`.

> **Detekce záměrně nevychází ze jména složky, ale z toho, kde se soubory našly.** Přejmenovaná složka (`2019-07 HR` → `2019-07-Chorvatsko`) se proto detekuje stejně spolehlivě jako přesunutá — shoda jmen souborů je silnější důkaz než shoda jména adresáře. Podobnost jmen složek vstupuje jen jako **bonusový signál ve skórování** (§5.3, +8) a jako informace v UI: pokud se jména liší, zobraz v dialogu řádek „⚠ Cílová složka má jiný název — ověř, že jde o tutéž kolekci fotek". To je situace, kdy `auto_threshold` nestačí a rozhodnout má člověk.

**Řešení (v pořadí preference):**
1. **Repoint DB** — jeden `UPDATE AgLibraryFolder SET pathFromRoot=…, rootFolder=…` místo N updatů souborů. Rychlé, bezpečné, nehýbe s daty. Pozor: pokud `T` už v katalogu existuje jako jiná složka, nelze repointovat → místo toho N× `UPDATE AgLibraryFile SET folder`.
2. **Přesun souborů zpět** — když uživatel chce zachovat původní strukturu.

### 7.2 Vzor B — rozsypaná složka (`SCATTERED`)

**Detekce:** soubory z jedné zdrojové složky `S` jsou nalezeny ve `k` různých cílových složkách, `k ≥ 5`, a medián počtu souborů na cílovou složku ≤ 2.

Typický původ: export/kopírování po jedné, nebo software, který každou fotku uloží do vlastního podadresáře.

**Řešení:** nabídnout **konsolidaci** — přesunout všechny do `S` (pokud existuje) nebo do nově vytvořeného adresáře. Plán musí předem detekovat kolize jmen v cíli a nabídnout strategii (přeskočit / přejmenovat `_1` / vybrat ručně).

> **Implementace M6 (`src/cluster/scattered.rs::plan_consolidate`):** kolize jmen v cíli se řeší stejným pre-flightem jako každý jiný přesun (`catalog::apply`, §8 bod 0.4) — odmítnutí + nahlášení, nikdy tiché přeskočení ani přepsání. Strategie „přejmenovat `_1`" nebo „vybrat ručně" NENÍ implementovaná — vyžadovala by vlastní interaktivní dialog kolizí; odmítnutí je bezpečné, i když míň pohodlné. `S` musí v katalogu vždy existovat (je to `file.expected_path`, odvozené přímo z katalogu) — větev „nově vytvořený adresář" proto v M6 odpadá, není potřeba.

### 7.3 Vzor C — album rozházené po discích (`ALBUM_SCATTERED`)

**Detekce:** přes `AgLibraryCollectionImage` — kolekce, jejíž fotky jsou fyzicky v ≥ 10 různých složkách a zároveň ≥ 30 % z nich chybí.

**Řešení:** návrh sloučení do jednoho adresáře pojmenovaného podle kolekce. Toto je destruktivnější operace (mění strukturu na disku, ne jen opravuje) — vyžaduj explicitní dvojí potvrzení a nikdy ji nezahrnuj do "Auto".

> **Implementace M6 (`src/cluster/album_scattered.rs`):** DETEKCE je hotová, ale schéma kolekcí (`AgLibraryCollection`/`AgLibraryCollectionImage`/`Adobe_images`) není ověřené na reálném katalogu (CLAUDE.md §6) — `catalog::schema::has_collection_schema` proto detekci tiše vypne, když tabulky/sloupce nesedí, místo aby nad neověřeným tvarem hádala. **Fyzické sloučení (řešení) NENÍ implementováno** — spec sama tuhle operaci označuje za nejdestruktivnější v celém nástroji a vyžaduje vlastní dialog s dvojím potvrzením i promyšlenou volbu, kam přesně "adresář pojmenovaný podle kolekce" založit (otevřená otázka, kterou spec nedořešuje). V panelu návrhů se návrh zobrazí jen informativně.

### 7.4 Vzor D — změna písmene disku / kořene (`ROOT_REMAP`)

Změna písmene externího disku (`E:` → `F:`) je v praxi nejčastější příčina „ztracených" fotek vůbec. Řeš ji **před jakýmkoli skenováním**, protože skenovat kvůli ní celý disk je absurdní.

#### 7.4.1 Rychlá sonda svazků (běží při startu, < 1 s)

Pro každý `AgLibraryRootFolder`, jehož `absolutePath` neexistuje:

1. Vezmi ze `AgLibraryFile` **5–10 vzorových souborů** z různých podsložek toho rootu (např. `ORDER BY random() LIMIT 10`) a jejich `pathFromRoot + idx_filename`.
2. Vyjmenuj připojené svazky (`GetLogicalDrives` + `FindFirstVolume`, včetně namountovaných bez písmene).
3. Pro každý svazek zkus `exists(<svazek>/<zbytek původní cesty za písmenem>/<vzorek>)`. To je několik desítek `stat()` volání celkem.
4. Shoda ≥ 80 % vzorků na jednom svazku → nabídni **jednoklikovou opravu**: `UPDATE AgLibraryRootFolder SET absolutePath = 'F:/Foto/'`.

Zkoušej i variantu, kdy se změnila i podcesta (disk připojený jako `F:/` místo `F:/Foto/`) — tj. hledej vzorek pod několika prefixy: `<svazek>/`, `<svazek>/<poslední komponenta původní cesty>/`.

#### 7.4.2 Identifikace disku podle sériového čísla

Bylo by ideální porovnat Volume Serial Number uloženého disku s připojenými svazky. **Neimplementuj to podle domněnky:** není potvrzeno, že LrC serial number v katalogu vůbec drží. Postup:

1. Prozkoumej katalog: `SELECT name FROM sqlite_master WHERE name LIKE '%olume%' OR name LIKE '%rive%';` a projdi sloupce `AgLibraryRootFolder` (zejména `name` a `relativePathFromCatalog`).
2. **Pokud se tam identifikátor svazku najde** → porovnávej ho s `GetVolumeInformationW` (`VolumeSerialNumber`) a nabídni opravu okamžitě, i bez sondy.
3. **Pokud ne** → sonda ze 7.4.1 dosahuje prakticky téhož výsledku za srovnatelnou cenu. Neztrácej tím čas.

Výsledek zjištění zapiš do kódu jako komentář, ať se to nemusí zkoumat znovu.

#### 7.4.3 Obecná detekce z výsledků matchingu

Pokud sonda selže a index už běžel: ≥ 90 % chybějících souborů z jednoho rootu nalezeno pod jedním společným prefixem s identickou zbývající strukturou → tentýž `UPDATE`, jen odvozený z dat místo ze sondy.

> **Pořadí vyhodnocení:** D → A → B → C → jednotlivé soubory. Každý vyřešený vzor odebere soubory z fronty pro nižší úrovně.

**Implementace M5, odchylky/upřesnění (`src/cluster/{folder_moved,scattered,root_remap}.rs`, `src/catalog/apply.rs`):**

* **§7.4.2 (sériové číslo svazku) NENÍ implementováno.** Vyžaduje ověřit na reálném katalogu, jestli `AgLibraryRootFolder` vůbec drží identifikátor svazku - bez přístupu k reálnému katalogu by to byl přesně ten druh hádání, který CLAUDE.md §6 zakazuje. Spec sama u tohoto bodu říká, že sonda §7.4.1 "dosahuje prakticky téhož výsledku za srovnatelnou cenu" - nástroj se na ni (+ §7.4.3) spoléhá. Zbytek vzoru D (§7.4.1 sonda, §7.4.3 odvození z MATCH) je hotový.
* **Vzor A "Repoint DB" (bod 1):** implementováno jako nová DB operace `DbOpTemplate::RemapFolder` (`UPDATE AgLibraryFolder SET pathFromRoot, rootFolder`). Když cílová složka `T` v katalogu už existuje jako JINÁ složka, `catalog::apply::plan_folder_repoint` to detekuje (`ManualResolveError::TargetFolderAlreadyExists`) a volající (`app::state::apply_folder_repoint`) automaticky spadne na N× `UpdateFileFolder` - přesně fallback, který spec popisuje, ale bez dalšího dotazu na uživatele (potvrzovací dialog na to upozorňuje předem).
* **Vzor A "Přesun souborů zpět" (bod 2) NENÍ implementován.** Je to fyzický přesun, ne DB-only operace - patří do M6 spolu se skupinami se sidecary (§3.5), na které je celá přesunová infrastruktura navázaná.
* **Vzor B (SCATTERED) je v M5 jen DETEKCE**, zobrazená v panelu návrhů jako informativní karta bez akčního tlačítka. "Konsolidace" (fyzický přesun N souborů do jednoho cíle vč. řešení kolizí jmen) potřebuje stejnou M6 infrastrukturu jako vzor A bod 2 - to odpovídá milníkové tabulce (§11), kde M5 řádek nemá "konsolidace" v DoD, ale M6 řádek ano.
* **80% práh u vzoru A a medián u vzoru B se počítají z "nalezených kandidátů", ne ze všech `n` chybějících souborů ve složce** - doslovné čtení "≥ 80 % nalezených kandidátů... leží v T" (§7.1) bere jmenovatel jako "soubory, které vůbec nějakého kandidáta mají", ne `n`. Soubor bez jakéhokoli kandidáta tedy nesnižuje poměr, jen ho vynechá ze jmenovatele.

---

## 8. Fáze APPLY — zápis

Filesystém a SQLite nesdílejí transakci. Jediné korektní řešení je **dvoufázový commit s explicitní kompenzací** — ne „nejdřív jedno, pak druhé a snad to vyjde".

Klíčové pravidlo: **SQL příkazy se generují až z výsledku FS fáze, ne před ní.** Transakce se otevírá teprve tehdy, když už je jisté, které přesuny doopravdy proběhly.

```
FÁZE 0 — PŘÍPRAVA (nic se nemění)
  0.1  Ověř znovu zámky katalogu (LrC se mohl mezitím otevřít).
  0.2  Vytvoř zálohu, pokud pro tuto session ještě není.
  0.3  Sestav Plan { fs_ops: Vec<FsOp>, db_ops_template: Vec<DbOpTemplate> }
       — db_ops_template drží vazbu na fs_op, ale ještě NEJSOU to SQL příkazy.
  0.4  Pre-flight: existuje zdroj, existuje cíl(adresář), je místo,
       není kolize jmen, není soubor zamčený (zkusit otevřít pro zápis).
       Cokoli selže → vyřaď z plánu HNED a ukaž uživateli před startem.
  0.5  Zapiš celý plán do journal.jsonl se stavem "planned".

FÁZE 1 — FILESYSTÉM
  1.1  Prováděj fs_ops po skupinách (soubor + jeho sidecary, §3.5).
  1.2  Každá úspěšná skupina → journal "fs_done" + zařaď do `committed_fs`.
  1.3  Každá neúspěšná skupina → journal "fs_failed" + zařaď do `failed`
       a ODSTRAŇ odpovídající db_op_template z plánu.
  1.4  Neukončuj běh při první chybě. 499 z 500 přesunů je platný výsledek —
       poslední se prostě neprojeví ani v DB.

FÁZE 2 — DATABÁZE
  2.1  Ze `committed_fs` + z DB-only operací (repoint, §7.1) vygeneruj SQL.
  2.2  BEGIN IMMEDIATE
  2.3    INSERT chybějících AgLibraryRootFolder / AgLibraryFolder řádků.
  2.4    UPDATE AgLibraryFile / AgLibraryFolder.
  2.5    Kontrolní dotazy (§8.1). Nesplnění → ROLLBACK → FÁZE 3.
  2.6  COMMIT

FÁZE 3 — KOMPENZACE (jen když FÁZE 2 selhala)
  3.1  Přesuň všechny soubory z `committed_fs` ZPĚT na původní místa.
  3.2  Ověř každý návrat hashem/velikostí; co se nepodaří vrátit,
       vypiš uživateli jako vyžadující ruční zásah (a NEmaž nic).
  3.3  Journal → "compensated" / "compensation_failed".
  3.4  Ukaž uživateli, že katalog je nedotčený a soubory jsou zpět.

FÁZE 4 — DOKONČENÍ
  4.1  Journal → "done".
  4.2  Znovu spusť VERIFY nad dotčenými soubory a ukaž výsledek.
  4.3  Souhrn: X vyřešeno, Y selhalo (s důvody), Z přeskočeno.
```

**Proč tímto pořadím:** kdyby se commitovala DB první a přesun pak selhal, katalog by ukazoval na cestu, kam se soubor nikdy nedostal — tedy přesně ta chyba, kterou nástroj má opravovat. Opačně je nejhorší možný výsledek dočasně přesunutý soubor, o kterém máme kompletní záznam v journalu.

**Dávkování:** `max_ops_per_batch` (default 5000) rozděluje plán na dávky, každá s vlastním kompletním cyklem 0–4. Uživatel může mezi dávkami přerušit a nezůstane po něm nekonzistentní mezistav. Progress bar ukazuje dávku i celek.

**Zámky souborů:** před přesunem zkus soubor otevřít s `FILE_SHARE_READ` (bez `SHARE_WRITE`). Když se to nepovede, soubor drží jiný proces (LrC, prohlížeč, antivirus, indexer) — vyřaď ho ve fázi 0.4, ať se to nezjistí až uprostřed běhu.

### 8.1 Kontrolní dotazy před commitem

```sql
-- žádný soubor nesmí ukazovat na neexistující složku
SELECT count(*) FROM AgLibraryFile f
  LEFT JOIN AgLibraryFolder fo ON f.folder = fo.id_local
 WHERE fo.id_local IS NULL;          -- musí být 0

-- žádná složka na neexistující root
SELECT count(*) FROM AgLibraryFolder fo
  LEFT JOIN AgLibraryRootFolder rf ON fo.rootFolder = rf.id_local
 WHERE rf.id_local IS NULL;          -- musí být 0

-- celkový počet souborů se nesmí změnit
SELECT count(*) FROM AgLibraryFile;  -- == hodnota před transakcí
```

Nesplnění → `ROLLBACK` a hlasitá chyba.

---

## 9. Journal a undo

`journal.jsonl` vedle katalogu, jeden JSON objekt na řádek:

```json
{"ts":"2026-08-20T10:12:03Z","session":"a1b2","seq":41,"state":"done",
 "op":"db_update","table":"AgLibraryFile","id_local":88213,
 "before":{"folder":1204},"after":{"folder":9911}}

{"ts":"2026-08-20T10:12:04Z","session":"a1b2","seq":42,"state":"done",
 "op":"db_insert","table":"AgLibraryFolder","id_local":9911,
 "after":{"pathFromRoot":"HR2019/","rootFolder":3,"id_global":"9F2C…"}}

{"ts":"2026-08-20T10:12:09Z","session":"a1b2","seq":43,"state":"done",
 "op":"file_move","src":"E:/dump/IMG_1234.CR2","dst":"C:/Foto/2019/IMG_1234.CR2",
 "size":25231360,"mtime":1563091200,"hash_blake3":"7c9e…"}
```

**Undo:** čti journal pozpátku pro danou session, aplikuj inverzní operaci (`db_update` → zapiš `before`; `db_insert` → `DELETE` jen pokud na řádek nic neukazuje; `file_move` → přesun zpět po ověření hashe). Undo běží ve vlastní transakci a sám se zapisuje do journalu jako nová session.

Vedle toho `run.log` (tracing) s plným průběhem včetně zamítnutých kandidátů — hodí se při ladění skórování.

---

## 10. Výkonové cíle

| Operace | Cíl (lokální NVMe) |
|---|---|
| EXTRACT 30k řádků | < 1 s |
| VERIFY 30k souborů / ~2k složek | < 5 s |
| INDEX přes Everything IPC | < 2 s |
| INDEX vlastní MFT scan, 2M souborů | < 15 s |
| MATCH 5k chybějících × index | < 2 s |
| MD5 verifikace 100 souborů á 25 MB | < 10 s (paralelně) |
| UI: scroll v tabulce 30k řádků | 60 fps |

---

## 11. Milníky

| # | Obsah | Definition of done |
|---|---|---|
| M0 | Read-only jádro: otevření lrcat, schema guard, EXTRACT, VERIFY, sonda svazků (§7.4.1), výpis do CLI | Vypíše seznam chybějících souborů; změna písmene disku odhalena bez skenování; na katalog nikdy nezapíše |
| M1 | GUI skeleton: dvoupanel, strom, tabulka, filtry | Zobrazí výsledky M0, plynulý scroll |
| M2 | INDEX (walk + Everything IPC) a MATCH se skórováním | Ke každému chybějícímu souboru kandidáti a skóre |
| M3 | Záloha (vč. kontroly místa), journal, APPLY jako two-phase commit, kontrolní dotazy | Katalog po zápisu otevřitelný v LrC, fotky viditelné; test kompenzace prochází |
| M4 | Dialog duplikátů + „použít pro zbytek složky" | Vyřešení 500 souborů bez 500 kliknutí |
| M5 | CLUSTER: ROOT_REMAP, FOLDER_MOVED, SCATTERED + panel návrhů | Přesun celé složky opraven jedním UPDATE |
| M6 | Přesuny souborů vč. skupin se sidecary (§3.5), konsolidace, ALBUM_SCATTERED, UNDO | Undo vrátí katalog i soubory do původního stavu; `.xmp` nikdy nezůstane pozadu |
| M7 | MFT scan, cache indexu, EXIF náhledy | Index 2M souborů pod 15 s |

---

## 12. Testy

* **Fixture katalog:** vytvoř skriptem malý `.lrcat` (kopie reálného, ořezaná) + strom testovacích souborů. Testy nikdy nesmí běžet nad produkčním katalogem.
* **Property test:** pro každý scénář (přesun složky, rozsypání, změna disku) proveď APPLY + UNDO a ověř, že SHA256 katalogu i stromu souborů odpovídá výchozímu stavu.
* **Integrity test:** po každém APPLY spusť `PRAGMA integrity_check` a kontrolní dotazy z §8.1.
* **Reálný test na konci:** katalog po zápisu otevřít v Lightroom Classic a ověřit, že fotky nemají "!" a náhledy sedí. Toto nelze automatizovat — zařaď jako povinný manuální krok v M3.
* **Fuzz cest:** diakritika (NFC vs. NFD, §16.3), mezery a tečky na konci jmen, `\\?\` dlouhé cesty > 260 znaků (§16.1), UNC cesty, symlinky, junctiony, nepárové surrogáty v `OsString`.
* **Test kompenzace:** vynuť selhání SQLite `COMMIT` (např. read-only atribut na katalogu nastavený mezi fází 1 a 2) a ověř, že se všech N přesunutých souborů vrátilo zpět bit-po-bitu a katalog je nedotčený. Tento test je povinný — je to jediná cesta, jak ověřit §8 fázi 3.
* **Test částečného selhání:** zamkni jeden soubor uprostřed dávky 500 přesunů a ověř, že 499 projde, katalog obsahuje přesně 499 updatů a zamčený soubor je hlášený s důvodem.
* **Test sidecarů:** RAW + `.xmp` + `.wav`; ověř, že se přesunou všechny tři nebo žádný. Zvlášť otestuj variantu RAW+JPEG jako dva samostatné `AgLibraryFile` řádky.
* **Test virtuálních kopií:** katalog s VK; počet souborů ve VERIFY musí odpovídat `AgLibraryFile`, ne `Adobe_images`.
* **Test časových tolerancí:** tentýž soubor na NTFS a na exFAT (SD karta), přes přechod letního času; skórování musí obě varianty uznat.

---

## 13. Rizika a jak je zmírnit

| Riziko | Zmírnění |
|---|---|
| Přímý zápis do `.lrcat` není Adobe podporovaný | Záloha + integrity check + undo + povinný manuální test v LrC |
| Schéma se mezi verzemi LrC liší | Schema guard, žádný `SELECT *`, hlasité selhání při neshodě |
| LrC 11+ ukládá část dat do `.lrcat-data` | Zálohovat oba soubory. Cesty ke složkám jsou v `.lrcat`, ale ověř to na svém katalogu. |
| Nesprávný kandidát → fotka ukazuje na cizí soubor | Práh skóre, MD5 při remíze, dry-run, undo |
| Přesun souborů, které LrC eviduje jinde | Před přesunem ověř, že cílové jméno není v katalogu obsazeno jiným `id_local` |
| Sidecar soubory (`.xmp`) zůstanou pozadu | Skupinový přesun podle `sidecarExtensions` + `baseName`, atomicky, viz §3.5 |
| Kandidát ze zálohy LrC | Vyloučení v indexeru + tvrdá penalizace, §15 |
| Cloud placeholder stažen omylem | Detekce atributů, žádné hashování placeholderů, §5.1.1 |
| Selhání uprostřed dávky přesunů | Two-phase commit s kompenzací, §8 |
| Katalog z macOS | Detekce POSIX cest, dialog ruční mapování rootů, §3.1 |
| Cesty > 260 znaků selžou ve WinAPI | Verbatim `\\?\` prefix v jediném konverzním bodě, §16.1 |
| Falešná selhání kvůli antiviru | Retry s backoffem a kontrolou idempotence, §16.2 |
| Diakritika z macOS (NFD) rozbije párování | Normalizované klíče pro porovnání, bajtově přesné cesty pro I/O, §16.3 |
| Síťové disky, pomalé I/O | Omezený thread pool, cancelovatelné operace, progress |

---

## 14. Konfigurace

`config.toml` vedle binárky:

```toml
[scan]
roots = []                 # prázdné = odvodit z AgLibraryRootFolder
threads_local = 16
threads_network = 4
exclude = [
  "$RECYCLE.BIN",
  "System Volume Information",
  "*/*.lrdata/*",              # Previews, Smart Previews, Helper.lrdata
  "*/Backups/*",               # automatické zálohy katalogu LrC
  "*/LrCatFix-backups/*",      # naše vlastní zálohy
  "*/Lightroom/*Backups*/*",
  "*/.Trash*/*", "*/Trash/*",
  "*/AppData/Local/Temp/*",
  "*/Adobe/CameraRaw/*",
  "*/node_modules/*", "*/.git/*",
]

[match]
auto_threshold = 70
tie_margin = 15
use_md5 = "on_tie"         # never | on_tie | always
use_mtime = "auto"         # auto = kalibrovat podle §5.4
folder_name_similarity = true   # Levenshtein bonus, §5.3

[cloud]
hash_placeholders = false  # nikdy nestahovat z cloudu kvůli hashi
warn_on_hydrate_mb = 500   # nad tento objem vyžadovat potvrzení

[safety]
require_backup = true
dry_run_default = true
max_ops_per_batch = 5000
```

---

## 15. Co explicitně NEDĚLAT

* Neměnit náhledy (`Previews.lrdata`, `Smart Previews.lrdata`) — LrC si je dogeneruje sám.
* Nesahat na `Adobe_images`, vývojová nastavení, historii úprav, klíčová slova, kolekce. Nástroj opravuje **jen cesty**.
* Nemazat žádný soubor na disku. Ani prázdné složky po konsolidaci — jen je vypsat a nechat rozhodnutí na uživateli.
* Nepokoušet se o zápis, když je katalog otevřený v LrC — ani "jen na chvíli".
* **Nikdy nenabízet jako kandidáta soubor ze zálohy Lightroomu** (`*/Backups/`, `*.lrdata`, rozbalené zálohy katalogu). Fotka nalezená uvnitř měsíc staré zálohy vypadá jako perfektní shoda a je to past — katalog by pak ukazoval do adresáře, který LrC nebo uživatel při dalším úklidu smaže. Vyloučení patří do indexeru (§14), ne až do skórování; a `−100` bodů v §5.3 je jen druhá pojistka pro cesty, které filtr nezachytí.
* Nestahovat data z cloudu bez explicitního potvrzení uživatele (§5.1.1).
* Nespouštět aplikaci trvale se zvýšenými právy (§5.2).

---

## 16. Implementační pasti (Windows + Rust)

Tři problémy, které se neprojeví na vývojářském stroji s krátkými ASCII cestami, ale spolehlivě se objeví u reálného katalogu s 30 000 fotkami. Řeš je od začátku, ne až podle bug reportů.

### 16.1 Dlouhé cesty (> 260 znaků)

Cesty typu `D:\Foto\2019\2019-07 Chorvatsko - dovolená s rodinou\originály\den 3 - Plitvická jezera\IMG_1234.CR2` limit `MAX_PATH` překročí snadno, zvlášť po konsolidaci složek, kde se cesty prodlužují.

* **Vlastní volání WinAPI** (`MoveFileExW`, `GetFileAttributesW`, `CreateFileW`, `DeviceIoControl`) prefix `\\?\` **nedostanou automaticky** — musíš ho přidat sám. Rust `std::fs` si verbatim cesty řeší interně, ale jakmile saháš přes crate `windows`, jsi na to sám.
* Převod na verbatim tvar má tvrdá pravidla, jinak API selže s matoucí chybou:
  * lokální cesta → `\\?\C:\Foto\...`
  * UNC cesta `\\nas\photo\...` → `\\?\UNC\nas\photo\...`
  * **jen zpětná lomítka** — `\\?\C:/Foto` nefunguje
  * cesta musí být **plně kvalifikovaná a už normalizovaná** — verbatim prefix vypíná normalizaci OS, takže `.`, `..` a dvojitá lomítka se nevyřeší a cesta selže
  * délka jedné komponenty (jména souboru/složky) zůstává 255 znaků i s prefixem
* **Pomocná funkce** `fn to_verbatim(p: &Path) -> Vec<u16>` — jediné místo v kódu, kudy cesty do WinAPI vstupují. Nikde jinde prefix neřeš.
* Verbatim cesty **nikdy nezobrazuj v UI** a nepředávej je do `ShellExecuteW("/select,...")` — Průzkumník je nezpracuje. V UI a pro shell používej čistý tvar.
* Do manifestu přidej `longPathAware` a v dokumentaci zmiň registry `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled`. Nespoléhej na to, že je zapnuté — verbatim prefix funguje bez ohledu na registry.
* Verbatim prefix navíc vypíná ořezávání koncových teček a mezer. Složka `Foto ` (s mezerou na konci) jde přes `\\?\` otevřít, běžnou cestou ne. To je při čtení z MFT reálný scénář.

### 16.2 Retry logika u I/O

Pre-flight kontrola zámků (§8, fáze 0.4) nestačí. Windows Defender skenuje čerstvě zapsaný soubor a drží ho desítky až stovky milisekund; stejně se chovají zálohovací agenti, indexer a cloudoví klienti. Bez opakování bude v každé větší dávce několik falešných selhání.

```rust
const RETRYABLE: &[u32] = &[
    32,   // ERROR_SHARING_VIOLATION
    33,   // ERROR_LOCK_VIOLATION
    5,    // ERROR_ACCESS_DENIED  (často jen dočasné od AV)
    1224, // ERROR_USER_MAPPED_FILE
    170,  // ERROR_BUSY
];
```

* **Backoff:** 5 pokusů, `50 / 100 / 200 / 400 / 800 ms` + náhodný jitter ±25 %. Celkem < 2 s na soubor v nejhorším případě, což je při 500 souborech přijatelné a v drtivé většině případů projde napoprvé.
* **Neopakuj** `ERROR_FILE_NOT_FOUND` (2), `ERROR_PATH_NOT_FOUND` (3), `ERROR_DISK_FULL` (112), `ERROR_ALREADY_EXISTS` (183), `ERROR_NOT_SAME_DEVICE` (17). Ty se opakováním nespraví a jen zdržují.
* **Idempotence je podmínka.** `MoveFileExW` je atomické, ale odpověď se může ztratit. Před dalším pokusem ověř skutečný stav: cíl existuje a zdroj ne → operace **proběhla**, vrať `Ok`, neopakuj. Bez této kontroly hrozí, že retry přepíše cíl nebo nahlásí selhání úspěšné operace a spustí zbytečnou kompenzaci.
* Retry logiku dej do **jednoho wrapperu** `fn with_retry<T>(op: impl Fn() -> Result<T>) -> Result<T>` a používej ho pro přesun, kopii, mazání i otevření k hashování. Neduplikuj ji na deseti místech.
* Do journalu zapiš počet pokusů. Soubor, který uspěl až napočtvrté, je signál k prošetření (AV, síť, umírající disk) a v souhrnu ho zmiň.
* **Kompenzační fáze (§8 fáze 3) používá stejný retry** — návrat souboru zpět je přesně ten okamžik, kdy je disk nejvytíženější a zámky nejpravděpodobnější.

### 16.3 Unicode normalizace (NFD ↔ NFC)

macOS ukládá jména souborů v **NFD** (`č` = `c` + U+030C), Windows a Linux v **NFC** (`č` = U+010D). Bajtově jsou to různé řetězce, vizuálně identické. Katalog přenesený z Macu, fotky ze SD karty formátované na Macu nebo obsah cloudové složky synchronizované z Macu → porovnání jmen tiše selže a nástroj ohlásí soubory jako chybějící, i když leží přesně tam, kde mají.

**Pravidlo, které nesmíš porušit:** normalizuj **jen pro porovnání**, nikdy pro I/O.

```rust
// klíč do NameIndex a pro veškeré porovnávání
fn match_key(name: &OsStr) -> String {
    name.to_string_lossy()
        .nfc().collect::<String>()   // unicode-normalization
        .to_lowercase()              // Windows: case-insensitive
}
```

* Struktura souboru drž **vždy dvojici**: `path: PathBuf` (bajtově přesně tak, jak ji vrátil filesystém — tímhle se otevírá) a `key: String` (normalizovaný — tímhle se porovnává). Nikdy neskládej cestu k otevření z normalizovaného klíče: na NTFS jsou NFD a NFC jméno **dva různé soubory** a normalizovaným jménem soubor neotevřeš.
* `NameIndex.by_name` klíčuj přes `match_key`. Totéž aplikuj na `idx_filename` z katalogu i na jména vrácená z MFT scanu a z `read_dir` — jinak se index a katalog rozejdou.
* Porovnání složek v celé fázi CLUSTER (§7) běží nad normalizovanými klíči, včetně Levenshteinovy vzdálenosti — jinak by dekomponovaná diakritika uměle nafoukla vzdálenost a bonus `+8` by nikdy nesedl.
* **Zápis do katalogu:** `idx_filename` musí obsahovat jméno v tom tvaru, v jakém je na disku (bajtově přesně), `lc_idx_filename` jeho lowercase. Nezapisuj tam normalizovanou variantu — LrC by soubor přestal najít a vyrobil bys přesně ten problém, který opravuješ.
* **`OsString` na Windows je WTF-16** a může obsahovat nepárové surrogáty, které `to_string_lossy()` nahradí U+FFFD. Cesty proto nikdy neprožeň přes `String` cestou k I/O — pracuj s `OsStrExt::encode_wide()`. `to_string_lossy()` je v pořádku výhradně pro `match_key` a pro zobrazení v UI.
* V testech měj fixture s NFD jménem (`Plitvická.CR2` dekomponovaně) i NFC variantou téhož a ověř, že se spárují.
