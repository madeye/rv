use rv_core::QualityPreset;
use vnc::VncEncoding;

pub fn encodings_for(quality: QualityPreset) -> Vec<VncEncoding> {
    let mut list = match quality {
        QualityPreset::Best => vec![VncEncoding::Tight, VncEncoding::CopyRect],
        QualityPreset::Fast => vec![VncEncoding::Zrle, VncEncoding::CopyRect],
        QualityPreset::Auto => vec![
            VncEncoding::Tight,
            VncEncoding::Zrle,
            VncEncoding::Trle,
            VncEncoding::CopyRect,
        ],
    };
    list.push(VncEncoding::DesktopSizePseudo);
    // No `CursorPseudo`: the compositor has no local cursor layer, so the
    // server must keep drawing the pointer into the framebuffer itself.
    list.push(VncEncoding::Raw);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_includes_raw() {
        for q in [
            QualityPreset::Auto,
            QualityPreset::Best,
            QualityPreset::Fast,
        ] {
            let e = encodings_for(q);
            assert!(e.contains(&VncEncoding::Raw));
            assert!(e.contains(&VncEncoding::DesktopSizePseudo));
            assert!(!e.contains(&VncEncoding::CursorPseudo));
        }
    }
}
