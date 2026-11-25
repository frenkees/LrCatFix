# CLAUDE.md

Pracovní instrukce pro práci v tomto repozitáři. Čti celé před první změnou kódu.

**Detailní specifikace je v `PROJECT.md`.** Tento soubor říká *jak* pracovat, `PROJECT.md` říká *co* postavit. Když si odporují, platí `PROJECT.md` v otázce chování aplikace a `CLAUDE.md` v otázce postupu.

---

## 1. Co je tenhle projekt v jedné větě

Desktopový nástroj (Rust + egui), který v Lightroom Classic katalogu (`.lrcat`, SQLite) najde fotky s neplatnou cestou, dohledá je na disku a po schválení uživatelem opraví cesty v databázi.

**Uživatel má ~30 000 fotek a jeden reálný katalog, o který nesmí přijít.** To je kontext každého rozhodnutí, které tady uděláš.

---

## 2. Zlaté pravidlo

> **Poškozený katalog je horší než nefunkční nástroj.**

Když si nejsi jistý, jestli je operace bezpečná, **neudělej ji a zeptej se**. Nikdy nevol variantu „zkusíme to a uvidíme". Uživatelova knihovna fotek obsahuje roky práce, kterou nelze rekonstruovat.

Praktický dopad na tvoje rozhodování:

* Raději odmítnout zápis než ho provést s nejistotou.
* Raději hlasitá chyba než tichý fallback, který změní data.
* Raději pomalejší a ověřený postup než rychlý a chytrý.

---

## 3. Invarianty — porušení je vždy chyba

Tyhle body neporušuj ani dočasně, ani „jen pro test", ani když to zjednoduší kód:

1. **Nikdy nezapisuj do katalogu bez ověřené zálohy.** Záloha musí projít `PRAGMA integrity_check` a kontrolou velikosti, než se otevře zapisovací spojení.
2. **Nikdy nezapisuj do katalogu, který může být otevřený v LrC.** Kontroluj `.lrcat.lock`, `-journal`, `-wal` těsně před zápisem, ne jen při otevření.
3. **Nikdy neměň `journal_mode`, `page_size`, `encoding`, `user_version`.** Žádný `VACUUM`, žádný `PRAGMA writable_schema`.
4. **Nikdy nepiš `SELECT *`.** Vždy vyjmenované sloupce, vždy po schema guardu.
5. **Nikdy nemaž soubor na disku.** Ani prázdnou složku. Nástroj přesouvá a zapisuje, nemaže.
6. **Nikdy negeneruj SQL pro operaci se souborem dřív, než ta operace na disku prokazatelně proběhla** (two-phase commit, `PROJECT.md` §8).
7. **Nikdy nepoužij normalizovaný (NFC/lowercase) řetězec k otevření souboru.** Normalizace slouží výhradně k porovnání (§16.3).
8. **Nikdy nestahuj data z cloudu** (OneDrive placeholder) bez explicitního souhlasu uživatele.
9. **Každá změna, kterou nástroj provede, musí být v journalu před tím, než se provede.**
10. **Testy nikdy neběží nad produkčním katalogem.** Viz §7.

Pokud tě nějaký úkol nutí porušit invariant, je špatně zadaný úkol — řekni to, nehledej obchvat.

---

## 4. Příkazy

```bash
cargo build                      # debug build
cargo build --release            # release, tohle měř při výkonových cílech
cargo run -- --catalog tests/fixtures/small.lrcat

cargo test                       # unit + integration
cargo test -- --ignored          # testy vyžadující velký fixture katalog
cargo test --test compensation   # povinný test kompenzační fáze (§8)

cargo clippy --all-targets -- -D warnings
cargo fmt --check

cargo run -- --catalog <cesta> --headless --report out.json   # CLI režim, read-only
```

**Před každým commitem musí projít:** `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.

Výkonové cíle z `PROJECT.md` §10 měř **výhradně v release buildu**. Debug build je u hashování a walku 10–30× pomalejší a měření z něj je bezcenné.

---

## 5. Struktura repozitáře

```
src/
  main.rs           bootstrap, CLI args, tracing init
  config.rs         config.toml, defaults
  app/              egui vrstva — JEN UI, žádná business logika
    state.rs        AppState, jediný zdroj pravdy pro UI
    panel_tree.rs   levý panel (strom složek)
    panel_files.rs  pravý panel (tabulka souborů)
    panel_plans.rs  dolní panel (návrhy z cluster)
    dialogs/        duplikáty, sloučení složek, mapování rootů, potvrzení
  catalog/          všechno kolem .lrcat
    open.rs         zámky, schema guard, verze
    schema.rs       definice očekávaných tabulek/sloupců + ověření
    extract.rs      EXTRACT (§3.1)
    apply.rs        two-phase commit (§8)
    backup.rs       záloha vč. kontroly místa
    ids.rs          generování id_local / id_global
  fsops/
    paths.rs        verbatim \\?\, normalizace, match_key (§16.1, §16.3)
    verify.rs       dávkové ověření existence (§5.1)
    attrs.rs        cloud placeholdery, typ FS, volné místo (§5.1.1)
    moves.rs        přesuny, skupiny se sidecary, retry (§16.2)
  index/
    mod.rs          NameIndex, výběr backendu
    everything.rs   Everything IPC / SDK
    mft.rs          FSCTL_ENUM_USN_DATA (unsafe, izolovaně)
    walk.rs         jwalk fallback
    cache.rs        serializace indexu
  matching/
    score.rs        skórování kandidátů (§5.3)
    calibrate.rs    kalibrace času a md5 (§5.4)
  cluster/
    root_remap.rs   vzor D + sonda svazků (§7.4)
    folder_moved.rs vzor A
    scattered.rs    vzor B
    album.rs        vzor C
  journal/
    write.rs        jsonl zápis
    undo.rs         reverzní přehrání
tests/
  fixtures/         malé testovací katalogy + generátor stromu souborů
  compensation.rs   povinný test §8 fáze 3
```

**Hranice, kterou nepřekračuj:** `app/` nesmí obsahovat SQL ani přímé I/O. Volá jen funkce z `catalog/`, `fsops/`, `index/`. Důvod je testovatelnost — celá logika musí jít otestovat bez spuštění GUI.

---

## 6. Práce se schématem `.lrcat`

Toto je nejčastější zdroj chyb v tomhle projektu.

**Schéma LrC se mezi verzemi liší a `PROJECT.md` ho popisuje podle nejlepšího vědomí, ne podle jistoty.** Několik věcí je v něm explicitně označeno jako neověřených (globální čítač `id_local`, sloupec pro virtuální kopie, formát `externalModTime`, obsah `md5`, uložení identifikátoru svazku).

**Postup, když narazíš na neověřený detail:**

1. **Ověř dotazem na reálný katalog.** Ne z paměti, ne z dokumentace na internetu, ne z analogie s jinou verzí.
2. Napiš si k tomu jednorázový průzkumný skript do `tests/fixtures/explore/`.
3. **Výsledek zapiš do kódu jako komentář** s uvedením verze LrC, na které to bylo ověřeno.
4. Když ověřit nejde (nemáš katalog té verze), **napiš runtime check**, který situaci detekuje a operaci odmítne s jasnou hláškou. Nehádej.

```rust
// OVĚŘENO na LrC 13.2 (katalog uživatele, 2026-08):
// Adobe_images.copyName IS NOT NULL <=> virtuální kopie.
// masterImage ukazuje na id_local originálu.
```

Marker `TODO(verify)` používej pro věci, které je potřeba ověřit na reálném katalogu. Nikdy nenech `TODO(verify)` v kódu, který zapisuje do databáze.

---

## 7. Testovací data

* **Nikdy nespouštěj testy ani vývojové běhy nad produkčním katalogem uživatele.** Ani read-only. Cesta k produkčnímu katalogu se v repozitáři nesmí objevit ani v konfiguraci, ani v testech, ani v komentářích.
* Fixture katalogy jsou v `tests/fixtures/`. Generátor (`tests/fixtures/make.rs`) vytvoří katalog + odpovídající strom souborů, pak do něj programově zanese poškození (přesun složky, rozsypání, změna písmene disku).
* Každý test, který zapisuje, si dělá vlastní kopii fixture do `tempfile::TempDir`. Sdílený mutable fixture je zdroj flaky testů.
* Fixture musí obsahovat: diakritiku v NFC i NFD, cestu > 260 znaků, RAW+`.xmp`+`.wav` skupinu, RAW+JPEG jako dva `AgLibraryFile` řádky, virtuální kopii, prázdnou složku.

---

## 8. Konvence kódu

**Chyby**

* Knihovní moduly (`catalog/`, `fsops/`, …): `thiserror`, konkrétní typované chyby.
* Hranice aplikace (`main.rs`, `app/`): `anyhow` s kontextem.
* `.unwrap()` a `.expect()` **jen v testech** a při inicializaci konstant. V produkčním kódu je to důvod k zamítnutí změny. Výjimka: `expect()` s vysvětlujícím textem tam, kde je porušení skutečně programátorská chyba — a i tam raději `debug_assert!`.
* Panika v UI vlákně shodí aplikaci uprostřed operace. `app/` nesmí panikařit nikdy.

**Unsafe**

* `unsafe` je povolený **jen** v `index/mft.rs` a v tenkých FFI wrapperech ve `fsops/`.
* Každý `unsafe` blok má nad sebou komentář `// SAFETY:` vysvětlující, proč platí předpoklady.
* Žádný `unsafe` v business logice, v `app/`, v `catalog/`.

**Logování**

* `tracing`, ne `println!`. Úrovně: `error` = operace selhala, `warn` = něco přeskočeno/podezřelé, `info` = milníky fází, `debug` = rozhodnutí skórovače včetně zamítnutých kandidátů, `trace` = per-soubor detaily.
* Log nesmí obsahovat celé cesty na `info` úrovni ve výchozím nastavení — jen počty. Cesty patří na `debug`.

**Souběžnost**

* `rayon` pro CPU/IO paralelismus, ne ruční vlákna.
* Počet vláken pro I/O je konfigurovatelný a **liší se pro lokální a síťové svazky** (`PROJECT.md` §5.1).
* Dlouhé operace běží mimo UI vlákno a musí být **zrušitelné** — `CancellationToken` nebo `AtomicBool`, kontrolovaný v každé iteraci. Uživatel musí umět zastavit skenování 2M souborů.
* Progress se do UI hlásí přes kanál, ne sdíleným mutexem přes celý stav.

**Cesty**

* Typ `PathBuf` uvnitř, `Vec<u16>` až na hranici WinAPI.
* Konverzi na verbatim `\\?\` dělá **jediná funkce** `fsops::paths::to_verbatim`. Nikde jinde prefix neřeš.
* Porovnávání jen přes `fsops::paths::match_key`. Nikdy `a == b` na jménech souborů.

**Závislosti**

* Nepřidávej crate bez důvodu. Před přidáním se zeptej, pokud to není v seznamu v `PROJECT.md` §4.
* Žádný webview, žádný Electron-like stack, žádný async runtime (tokio) — projekt je CPU/IO bound a `rayon` stačí.

---

## 9. Jak postupovat při implementaci

Milníky M0–M7 jsou v `PROJECT.md` §11. Drž se jejich pořadí.

**Pravidla postupu:**

1. **M0 dokončit celé, než se sáhne na GUI.** Read-only jádro s CLI výstupem je základ, na kterém se dá všechno ostatní ověřit. GUI bez funkčního jádra je jen hezký obal na neznámé chování.
2. **Zápis do katalogu implementuj až po journalu a záloze**, ne obráceně. Pořadí v M3 není náhodné.
3. **Jedna změna = jeden milník nebo jeho část.** Neměň zároveň skórování, UI a SQL vrstvu.
4. **Než napíšeš kód proti databázi, spusť průzkumný dotaz** a podívej se na reálná data. Sloupec, který má podle názvu obsahovat čas, může obsahovat cokoliv.
5. **Po každém milníku aktualizuj `PROJECT.md`**, pokud se implementace odchýlila od plánu. Spec, který lže, je horší než žádný.

**Když narazíš na nejednoznačnost v zadání:** zeptej se. Neimplementuj obě varianty, nevymýšlej si výchozí chování u věcí, které mění data uživatele.

---

## 10. Definition of done

Změna je hotová, když:

* [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` procházejí
* [ ] žádný nový `.unwrap()` v produkčním kódu
* [ ] žádný nový `TODO(verify)` v zapisovací cestě
* [ ] nové chování má test (u zapisovacích operací i test undo/kompenzace)
* [ ] chybové stavy mají hlášku, kterou pochopí uživatel, ne jen `Err(e)` do logu
* [ ] dlouhá operace je zrušitelná a hlásí progress
* [ ] u operací nad katalogem: ověřeno, že po nich LrC katalog otevře (u M3+ manuálně)

---

## 11. Čeho si všímat u UI

* Režim **NÁHLED** vs. **ZÁPIS POVOLEN** musí být vidět na první pohled. Zápisový režim výrazně barevně odlišený. Uživatel nikdy nesmí být na pochybách, jestli právě mění data.
* Žádné tlačítko nesmí vyvolat nevratnou operaci na jedno kliknutí bez souhrnu toho, co se stane (kolik řádků DB, kolik souborů, kolik GB).
* Tabulka s 30 000 řádky musí být virtualizovaná (`egui_extras::TableBuilder`). Nikdy nerenderuj všechny řádky.
* Klávesnice je rovnocenná myši — cílový uživatel jede stylem Total Commander (`PROJECT.md` §6.4).
* Hlášky česky, technické detaily do logu. „Soubor je používán jiným programem" místo „ERROR_SHARING_VIOLATION (32)".

---

## 12. Co v tomhle projektu explicitně neděláme

* Neupravujeme náhledy (`Previews.lrdata`, `Smart Previews.lrdata`).
* Nesaháme na vývojová nastavení, historii úprav, klíčová slova, kolekce, hodnocení. **Nástroj opravuje jen cesty.**
* Neděláme správce fotek, prohlížeč, ani nástroj na organizaci knihovny. Rozsah je úzký záměrně.
* Nepodporujeme Lightroom CC (cloudový) — jen Lightroom Classic s lokálním `.lrcat`.
* Neimplementujeme automatický režim, který by bez potvrzení uživatele hromadně měnil katalog. Auto-match navrhuje, člověk schvaluje.
