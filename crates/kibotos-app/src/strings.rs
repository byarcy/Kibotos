//! All user-visible text. The UI is in Spanish; to add a language, copy the
//! `es` module, translate it, and select it in `t()`.

use kibotos_core::{Error, LimitKind};

pub mod es {
    pub const APP_NAME: &str = "Kibotós";

    pub const MENU_FILE: &str = "&Archivo";
    pub const MENU_OPEN: &str = "&Abrir...\tCtrl+O";
    pub const MENU_EXTRACT: &str = "&Extraer...\tCtrl+E";
    pub const MENU_EXIT: &str = "&Salir";
    pub const MENU_HELP: &str = "Ay&uda";
    pub const MENU_ABOUT: &str = "&Acerca de Kibotós";

    pub const COL_NAME: &str = "Nombre";
    pub const COL_SIZE: &str = "Tamaño";
    pub const COL_PACKED: &str = "Comprimido";
    pub const COL_MODIFIED: &str = "Modificado";

    pub const UP_ROW: &str = "..";
    pub const FILTER_CUE: &str = "Filtrar en todo el archivo (Ctrl+F)";

    pub const STATUS_READY: &str = "Arrastra un archivo comprimido aquí o pulsa Ctrl+O para abrirlo.";
    pub const STATUS_OPENING: &str = "Abriendo...";

    pub const OPEN_TITLE: &str = "Abrir archivo comprimido";
    pub const FILTER_ARCHIVES: &str = "Archivos comprimidos";
    pub const FILTER_ALL: &str = "Todos los archivos";

    pub const ERROR_TITLE: &str = "No se pudo abrir el archivo";
    pub const ABOUT_TITLE: &str = "Acerca de Kibotós";
    pub const ABOUT_TAGLINE: &str = "Gestor de archivos comprimidos rápido y ligero.";
    pub const SIGNATURE: &str = "byarcy 2026";

    pub const OK: &str = "Aceptar";
    pub const CANCEL: &str = "Cancelar";

    pub const PICK_DEST_TITLE: &str = "Elige la carpeta de destino";
    pub const PICK_DEST_OK: &str = "Extraer aquí";

    pub const PROGRESS_EXTRACTING: &str = "Extrayendo";
    pub const PROGRESS_PREPARING: &str = "Preparando...";
    pub const PROGRESS_CANCELLING: &str = "Cancelando...";

    pub const PASSWORD_TITLE: &str = "Contraseña";
    pub const PASSWORD_PROMPT: &str = "está protegido con contraseña. Escríbela para continuar:";
    pub const PASSWORD_WRONG: &str = "La contraseña no es correcta. Inténtalo de nuevo:";

    pub const CONFLICT_TITLE: &str = "El archivo ya existe";
    pub const CONFLICT_REPLACE: &str = "&Reemplazar";
    pub const CONFLICT_SKIP: &str = "&Omitir";
    pub const CONFLICT_ALL: &str = "Hacer lo mismo con los demás conflictos";
    pub const CONFLICT_EXISTING: &str = "Existente:";
    pub const CONFLICT_NEW: &str = "Nuevo:";

    pub const EXTRACT_ERROR_TITLE: &str = "No se pudo extraer";
    pub const EXTRACT_CANCELLED: &str = "Extracción cancelada.";
    pub const BLOCKED_TITLE: &str = "Elementos omitidos por seguridad";
    pub const BLOCKED_INTRO: &str = "Estos elementos no se extrajeron porque podrían escribir fuera de la carpeta de destino o no son archivos normales:";
    pub const BLOCKED_MORE: &str = "y otros";
    pub const REASON_TRAVERSAL: &str = "sale de la carpeta (..)";
    pub const REASON_ABSOLUTE: &str = "ruta absoluta";
    pub const REASON_RESERVED: &str = "nombre reservado de Windows";
    pub const REASON_EMPTY: &str = "nombre vacío";
    pub const REASON_LINK: &str = "enlace simbólico";

    pub const ERR_UNKNOWN_FORMAT: &str = "El archivo no es un formato comprimido compatible.";
    pub const ERR_CORRUPT: &str = "El archivo está dañado o incompleto.";
    pub const ERR_PASSWORD_REQUIRED: &str = "El archivo está protegido con contraseña.";
    pub const ERR_WRONG_PASSWORD: &str = "La contraseña no es correcta.";
    pub const ERR_MISSING_VOLUME: &str = "Falta un volumen del archivo:";
    pub const ERR_UNSUPPORTED: &str = "El archivo usa una función no compatible:";
    pub const ERR_IO: &str = "No se pudo leer el archivo:";
    pub const ERR_LIMIT_TOTAL: &str = "La extracción supera el tamaño máximo permitido.";
    pub const ERR_LIMIT_RATIO: &str =
        "La extracción se detuvo: el contenido crece de forma anómala (posible bomba de descompresión).";
    pub const ERR_LIMIT_ENTRY: &str = "Un elemento contiene más datos de los que declara su cabecera.";
    pub const ERR_CANCELLED: &str = "Operación cancelada.";
}

pub use es as t;

/// Formats an integer with Spanish thousands separators: 1234567 -> "1.234.567".
pub fn number(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}

pub fn items_summary(files: usize, dirs: usize) -> String {
    let f = if files == 1 { "archivo".to_string() } else { "archivos".to_string() };
    let d = if dirs == 1 { "carpeta".to_string() } else { "carpetas".to_string() };
    format!("{} {f}, {} {d}", number(files as u64), number(dirs as u64))
}

pub fn folder_items(n: usize) -> String {
    if n == 1 { "1 elemento".into() } else { format!("{} elementos", number(n as u64)) }
}

pub fn matches(n: usize, query: &str) -> String {
    let word = if n == 1 { "coincidencia" } else { "coincidencias" };
    format!("{} {word} de «{query}»", number(n as u64))
}

/// "12,3 MB" style sizes for progress and summaries.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.1} {}", v, UNITS[u]).replace('.', ",")
}

pub fn progress_detail(files_done: u64, files_total: u64, bytes_done: u64, bytes_total: u64, rate: f64) -> String {
    format!(
        "{} de {} archivos  ·  {} de {}  ·  {}/s",
        number(files_done),
        number(files_total),
        bytes(bytes_done),
        bytes(bytes_total),
        bytes(rate as u64)
    )
}

pub fn extracted(files: u64, total: u64, secs: f64, dest: &str) -> String {
    let f = if files == 1 { "archivo" } else { "archivos" };
    let secs = format!("{secs:.1}").replace('.', ",");
    format!("Extraídos {} {f} ({}) en {secs} s  →  {dest}", number(files), bytes(total))
}

pub fn conflict_question(name: &str) -> String {
    format!("Ya existe «{name}» en la carpeta de destino.")
}

pub fn password_prompt(archive: &str, wrong: bool) -> String {
    if wrong { t::PASSWORD_WRONG.to_string() } else { format!("«{archive}» {}", t::PASSWORD_PROMPT) }
}

pub fn blocked_reason(r: &kibotos_core::SkipReason) -> &'static str {
    use kibotos_core::{SkipReason, UnsafeReason};
    match r {
        SkipReason::Unsafe(UnsafeReason::Traversal) => t::REASON_TRAVERSAL,
        SkipReason::Unsafe(UnsafeReason::Absolute) => t::REASON_ABSOLUTE,
        SkipReason::Unsafe(UnsafeReason::ReservedName) => t::REASON_RESERVED,
        SkipReason::Unsafe(UnsafeReason::Empty) => t::REASON_EMPTY,
        SkipReason::Link => t::REASON_LINK,
        SkipReason::Exists => "",
    }
}

pub fn opened_in(format: &str, entries: usize, ms: f64) -> String {
    format!("{format} · {} elementos · abierto en {ms:.0} ms", number(entries as u64))
}

pub fn version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    match v.strip_suffix(".0") {
        Some(short) => format!("v{short}"),
        None => format!("v{v}"),
    }
}

pub fn about_text() -> String {
    format!("{} {}\n{}\n\n{}", t::APP_NAME, version(), t::ABOUT_TAGLINE, t::SIGNATURE)
}

/// User-facing message for a core error.
pub fn error_message(e: &Error) -> String {
    match e {
        Error::UnknownFormat => t::ERR_UNKNOWN_FORMAT.into(),
        Error::Corrupt(d) => format!("{}\n\n{d}", t::ERR_CORRUPT),
        Error::PasswordRequired => t::ERR_PASSWORD_REQUIRED.into(),
        Error::WrongPassword => t::ERR_WRONG_PASSWORD.into(),
        Error::MissingVolume(v) => format!("{} {v}", t::ERR_MISSING_VOLUME),
        Error::Unsupported(d) => format!("{} {d}", t::ERR_UNSUPPORTED),
        Error::Io(e) => format!("{} {e}", t::ERR_IO),
        Error::LimitExceeded(LimitKind::TotalSize) => t::ERR_LIMIT_TOTAL.into(),
        Error::LimitExceeded(LimitKind::Ratio) => t::ERR_LIMIT_RATIO.into(),
        Error::LimitExceeded(LimitKind::EntrySize) => t::ERR_LIMIT_ENTRY.into(),
        Error::Cancelled => t::ERR_CANCELLED.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number(0), "0");
        assert_eq!(number(999), "999");
        assert_eq!(number(1000), "1.000");
        assert_eq!(number(1234567), "1.234.567");
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(1536), "1,5 KB");
        assert_eq!(bytes(5 * 1024 * 1024), "5,0 MB");
    }

    #[test]
    fn version_is_short() {
        assert!(version().starts_with("v0."));
    }
}
