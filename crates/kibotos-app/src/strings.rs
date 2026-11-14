//! All user-visible text. The UI is in Spanish; to add a language, copy the
//! `es` module, translate it, and select it in `t()`.

use kibotos_core::{Error, LimitKind};

pub mod es {
    pub const APP_NAME: &str = "Kibotós";

    pub const MENU_FILE: &str = "&Archivo";
    pub const MENU_OPEN: &str = "&Abrir...\tCtrl+O";
    pub const MENU_EXTRACT: &str = "&Extraer...\tCtrl+E";
    pub const MENU_EXIT: &str = "&Salir";
    pub const MENU_TOOLS: &str = "&Herramientas";
    pub const MENU_INTEGRATE: &str = "&Integrar con Windows";
    pub const MENU_UNINTEGRATE: &str = "&Quitar integración";
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

    pub const PROGRESS_OPENING: &str = "Abriendo";
    pub const OPEN_FAILED: &str = "No se pudo abrir el archivo";
    pub const MODIFIED_TITLE: &str = "Archivos modificados";
    pub const MODIFIED_MAIN: &str = "Modificaste archivos abiertos desde el comprimido";
    pub const MODIFIED_TEXT: &str = "Kibotós todavía no puede guardar cambios dentro de un comprimido (llegará en la v0.2). Puedes guardar una copia de cada archivo junto al comprimido.";
    pub const SAVE_COPIES: &str = "&Guardar copias";
    pub const DISCARD: &str = "&Descartar cambios";
    pub const MODIFIED_SUFFIX: &str = "modificado";
    pub const COPIES_SAVED: &str = "Copias guardadas";
    pub const COPY_FAILED: &str = "No se pudo guardar la copia de";

    pub const CLI_USAGE: &str = "Uso:\n\n  kibotos.exe [archivo]\n      Abre el archivo comprimido.\n\n  kibotos.exe --extract-here archivo...\n      Extrae junto al archivo (en una carpeta con su nombre si tiene varios elementos sueltos).\n\n  kibotos.exe --extract-to-folder archivo...\n      Extrae en una carpeta con el nombre del archivo.\n\n  kibotos.exe --extract-dialog archivo...\n      Pregunta dónde extraer.\n\n  kibotos.exe --integrate | --unintegrate\n      Añade o quita el menú contextual y la opción «Abrir con» (solo para tu usuario).";

    // Windows integration (registry labels and messages)
    pub const REG_TYPE_NAME: &str = "Archivo comprimido";
    pub const REG_OPEN: &str = "Abrir con Kibotós";
    pub const REG_EXTRACT_HERE: &str = "Extraer aquí";
    pub const REG_EXTRACT_FOLDER: &str = "Extraer en una carpeta con su nombre";
    pub const REG_EXTRACT_DIALOG: &str = "Extraer ficheros...";
    pub const INTEGRATE_TITLE: &str = "Integración con Windows";
    pub const INTEGRATE_DONE: &str = "Kibotós se ha integrado con Windows para tu usuario (sin permisos de administrador).";
    pub const INTEGRATE_MENU: &str = "Clic derecho en un comprimido → «Kibotós». En Windows 11 está dentro de «Mostrar más opciones».";
    pub const INTEGRATE_DEFAULT: &str = "Para abrir los comprimidos con doble clic: clic derecho en uno → Abrir con → Elegir otra aplicación → Kibotós → Siempre. Windows no deja que un programa se ponga como predeterminado por su cuenta.";
    pub const INTEGRATE_PATH: &str = "Programa registrado (si lo mueves, vuelve a integrarlo):";
    pub const UNINTEGRATE_DONE: &str = "Se ha quitado la integración de Kibotós con Windows; no queda nada suyo en el registro.";
    pub const UNINTEGRATE_NOTE: &str = "Si lo habías elegido como programa predeterminado, Windows volverá a preguntar con qué abrir esos archivos.";
    pub const INTEGRATE_FAILED: &str = "No se pudo modificar el registro:";

    // Context menu of the list
    pub const CTX_OPEN: &str = "&Abrir\tEnter";
    pub const CTX_OPEN_WITH: &str = "Abrir &con...";
    pub const CTX_EXTRACT: &str = "&Extraer...\tCtrl+E";
    pub const CTX_EXTRACT_ALL: &str = "&Extraer todo...\tCtrl+E";
    pub const CTX_EXTRACT_NEXT: &str = "Extraer &junto al comprimido";
    pub const CTX_LOCATE: &str = "&Mostrar en su carpeta\tCtrl+Enter";
    pub const CTX_COPY_PATH: &str = "Copiar &ruta";
    pub const CTX_PROPERTIES: &str = "&Propiedades\tAlt+Enter";
    pub const STATUS_PATH_COPIED: &str = "Ruta copiada al portapapeles.";

    // Properties
    pub const PROP_TITLE: &str = "Propiedades";
    pub const PROP_NAME: &str = "Nombre:";
    pub const PROP_LOCATION: &str = "Ubicación:";
    pub const PROP_TYPE: &str = "Tipo:";
    pub const PROP_FOLDER: &str = "Carpeta";
    pub const PROP_FILE: &str = "Archivo";
    pub const PROP_LINK: &str = "Enlace simbólico";
    pub const PROP_CONTAINS: &str = "Contiene:";
    pub const PROP_SIZE: &str = "Tamaño:";
    pub const PROP_PACKED: &str = "Comprimido:";
    pub const PROP_MODIFIED: &str = "Modificado:";
    pub const PROP_ENCRYPTED: &str = "Cifrado:";
    pub const PROP_SELECTION: &str = "Selección:";
    pub const PROP_UNKNOWN: &str = "desconocido";
    pub const YES: &str = "Sí";
    pub const NO: &str = "No";

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
        "{} de {} elementos  ·  {} de {}  ·  {}/s",
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

/// "12,3 MB (12.912.345 bytes)"; exact bytes are useful in properties.
pub fn bytes_exact(n: u64) -> String {
    if n < 1024 { format!("{} bytes", number(n)) } else { format!("{} ({} bytes)", bytes(n), number(n)) }
}

/// "35 %" of the original size.
pub fn ratio(packed: u64, size: u64) -> String {
    if size == 0 { String::new() } else { format!("  ·  {} %", (packed as f64 * 100.0 / size as f64).round() as u64) }
}

pub fn selection_count(n: usize) -> String {
    if n == 1 { "1 elemento".into() } else { format!("{} elementos seleccionados", number(n as u64)) }
}

pub fn opened(name: &str) -> String {
    format!("Abierto «{name}»")
}

pub fn integrated(exe: &str) -> String {
    format!("{}\n\n•  {}\n\n•  {}\n\n{}\n{exe}", t::INTEGRATE_DONE, t::INTEGRATE_MENU, t::INTEGRATE_DEFAULT, t::INTEGRATE_PATH)
}

pub fn unintegrated() -> String {
    format!("{}\n\n{}", t::UNINTEGRATE_DONE, t::UNINTEGRATE_NOTE)
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
