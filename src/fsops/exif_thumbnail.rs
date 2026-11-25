//! Náhled z vloženého EXIF thumbnailu (PROJECT.md §6.2 "Náhled: vložený
//! EXIF thumbnail (u RAW) nebo dekódovaný JPEG") - dialog duplikátů u
//! výběru z několika kandidátů zásadně pomáhá vidět, o jakou fotku jde.
//!
//! Používá `kamadak-exif` (schváleno uživatelem pro M7, PROJECT.md §4
//! nemá EXIF/RAW parsing v seznamu - viz PROJECT.md poznámka u M4/M7).
//! Čte jen `JPEGInterchangeFormat`/`JPEGInterchangeFormatLength` v IFD1
//! (`In::THUMBNAIL`) - vestavěný JPEG náhled, který skoro každý RAW i JPEG
//! soubor má, ne dekódování samotného RAW obrazu (to `kamadak-exif` ani
//! neumí - je to čistě EXIF/TIFF parser, ne RAW dekodér).
//!
//! **Nikdy se nevolá na cloud placeholderu** (CLAUDE.md invariant 8) -
//! stejné pravidlo jako `fsops::hash::md5_hex`.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use exif::{In, Tag};
use thiserror::Error;

use crate::fsops::attrs::is_cloud_placeholder_path;
use crate::fsops::paths::to_verbatim_for_io;

#[derive(Debug, Error)]
pub enum ThumbnailError {
    #[error("nelze číst EXIF náhled cloud placeholderu (spustilo by se tím stažení)")]
    CloudPlaceholder,
    #[error("chyba I/O při čtení souboru: {0}")]
    Io(#[from] std::io::Error),
    #[error("chyba při parsování EXIF: {0}")]
    Exif(#[from] exif::Error),
    #[error("soubor nemá vložený EXIF náhled")]
    NoThumbnail,
}

/// Vytáhne vestavěný JPEG náhled jako syrové JPEG bajty - volající je může
/// rovnou předat dekodéru obrázků pro zobrazení (např. `egui::ColorImage`
/// po dekódování přes `image`/vestavěné `egui_extras` načítání, mimo
/// rozsah tohoto modulu).
pub fn extract_jpeg_thumbnail(path: &Path) -> Result<Vec<u8>, ThumbnailError> {
    if is_cloud_placeholder_path(path) {
        return Err(ThumbnailError::CloudPlaceholder);
    }

    let file = File::open(to_verbatim_for_io(path))?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader)?;

    let offset = exif
        .get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)
        .and_then(|f| f.value.get_uint(0))
        .ok_or(ThumbnailError::NoThumbnail)?;
    let length = exif
        .get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)
        .and_then(|f| f.value.get_uint(0))
        .ok_or(ThumbnailError::NoThumbnail)?;

    // `offset`/`length` jsou v EXIF/TIFF konvenci relativní k začátku TIFF
    // hlavičky - `Exif::buf()` je přesně ten úsek (zdokumentováno v
    // kamadak-exif: "Returns the slice that contains the TIFF data").
    let buf = exif.buf();
    let start = offset as usize;
    let end = start
        .checked_add(length as usize)
        .ok_or(ThumbnailError::NoThumbnail)?;
    if end > buf.len() {
        return Err(ThumbnailError::NoThumbnail);
    }

    Ok(buf[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use exif::experimental::Writer;
    use exif::{Field, Value};

    /// Ověřuje offset/length aritmetiku end-to-end proti SKUTEČNÉMU
    /// kódování `kamadak-exif` (žádné ručně spočítané bajty) - zapíše
    /// syntetický TIFF/EXIF soubor s vloženým "JPEG" náhledem přes crate
    /// vlastní `Writer` a přečte ho zpátky přes `extract_jpeg_thumbnail`.
    #[test]
    fn extracts_embedded_jpeg_thumbnail_round_trip() {
        let thumbnail_bytes = b"\xff\xd8\xffnot-a-real-jpeg-just-test-bytes\xff\xd9".to_vec();

        // IFD0 (PRIMARY) nesmí být prázdné, i když nás zajímá jen IFD1
        // (THUMBNAIL) - `Writer` to jinak odmítne jako neplatný TIFF.
        let image_desc = Field {
            tag: Tag::ImageDescription,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"test".to_vec()]),
        };
        let mut writer = Writer::new();
        writer.push_field(&image_desc);
        writer.set_jpeg(&thumbnail_bytes, In::THUMBNAIL);
        let mut buf = std::io::Cursor::new(Vec::new());
        writer
            .write(&mut buf, false)
            .expect("Writer::write nesmí selhat pro platná data");
        let tiff_bytes = buf.into_inner();

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synteticky.cr2");
        std::fs::write(&path, &tiff_bytes).unwrap();

        let extracted = extract_jpeg_thumbnail(&path).unwrap();
        assert_eq!(extracted, thumbnail_bytes);
    }

    #[test]
    fn file_without_exif_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nejsou_to_data.CR2");
        std::fs::write(&path, b"tohle neni EXIF ani TIFF").unwrap();

        let err = extract_jpeg_thumbnail(&path).unwrap_err();
        assert!(matches!(err, ThumbnailError::Exif(_)));
    }

    #[test]
    fn missing_file_is_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_jpeg_thumbnail(&tmp.path().join("neexistuje.CR2")).unwrap_err();
        assert!(matches!(err, ThumbnailError::Io(_)));
    }
}
