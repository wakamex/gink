//! Takes out of a file what it says beyond its picture: where and when a photo was taken, with
//! what, and by whom. Nothing is decoded and nothing encoded again: the parts that hold such things
//! are left out, or blanked where leaving them out would move what other parts point to.

fn be16(b: &[u8], at: usize) -> Option<usize> {
    b.get(at..at + 2).map(|v| u16::from_be_bytes([v[0], v[1]]) as usize)
}

fn be32(b: &[u8], at: usize) -> Option<usize> {
    b.get(at..at + 4).map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as usize)
}

/// The file as it is to be sent: a JPEG, PNG or WebP picture, or a video in an MP4 or QuickTime
/// file (`video`, since sound files of the same form keep their titles there), without its
/// metadata; any other file as it is. None for a picture or video too broken to be gone through,
/// which may still hold what should have been removed.
pub fn strip(bytes: Vec<u8>, video: bool) -> Option<Vec<u8>> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        jpeg(&bytes)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(&bytes)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        webp(&bytes)
    } else if video && bytes.get(4..8) == Some(b"ftyp") {
        mp4(bytes)
    } else {
        Some(bytes)
    }
}

/// A JPEG keeps its picture, its colour profile and which way up it is shown, and loses every
/// other application segment and comment, and whatever follows its end.
fn jpeg(b: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0xff, 0xd8];
    let mut at = 2;
    // The segments before the picture's data, each saying how long it is.
    loop {
        if *b.get(at)? != 0xff {
            return None;
        }
        let marker = *b.get(at + 1)?;
        if marker == 0xff {
            at += 1;  // (a fill byte)
            continue;
        }
        let end = at + 2 + be16(b, at + 2)?;
        let segment = b.get(at..end)?;
        let data = &segment[4.min(segment.len())..];
        match marker {
            // (the Exif block goes, but for how the camera was held, without which the picture is shown on its side)
            0xe1 if data.starts_with(b"Exif\0\0") => {
                if let Some(turn) = crate::signal::exif_orientation(&data[6..]).filter(|o| (2..=8).contains(o)) {
                    out.extend(b"\xff\xe1\0\x22Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0");
                    out.extend([turn as u8, 0, 0, 0, 0, 0, 0]);
                }
            }
            0xe0 | 0xee => out.extend(segment),
            0xe2 if data.starts_with(b"ICC_PROFILE\0") => out.extend(segment),
            0xe1..=0xef | 0xfe => {}
            _ => out.extend(segment),
        }
        at = end;
        if marker == 0xda {
            break;
        }
    }
    // The picture's data, in which a marker is a 0xff before anything but a zero; it ends at the
    // end-of-image marker, and what cameras put after that (other pictures, a short video) goes.
    let start = at;
    loop {
        at += b.get(at..)?.iter().position(|v| *v == 0xff)?;
        match *b.get(at + 1)? {
            0xd9 => break,
            0x00 | 0xd0..=0xd7 | 0x01 => at += 2,
            0xff => at += 1,
            _ => at += 2 + be16(b, at + 2)?,  // (the tables and scans of a picture stored in passes)
        }
    }
    out.extend(b.get(start..at + 2)?);
    Some(out)
}

/// A PNG loses its text, its Exif block and the time it was last changed, and whatever follows its end.
fn png(b: &[u8]) -> Option<Vec<u8>> {
    let mut out = b[..8].to_vec();
    let mut at = 8;
    loop {
        let end = at + 12 + be32(b, at)?;
        let chunk = b.get(at..end)?;
        let kind = &chunk[4..8];
        if !matches!(kind, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME") {
            out.extend(chunk);
        }
        if kind == b"IEND" {
            return Some(out);
        }
        at = end;
    }
}

/// A WebP loses its Exif and XMP parts, and its header stops saying that it has them.
fn webp(b: &[u8]) -> Option<Vec<u8>> {
    let mut out = b[..12].to_vec();
    let mut at = 12;
    while at < b.len() {
        let size = u32::from_le_bytes(b.get(at + 4..at + 8)?.try_into().ok()?) as usize;
        let end = (at + 8 + size + (size & 1)).min(b.len());
        let chunk = b.get(at..end)?;
        match &chunk[..4] {
            b"EXIF" | b"XMP " => {}
            b"VP8X" => {
                out.extend(chunk);
                let flags = out.len() - chunk.len() + 8;
                *out.get_mut(flags)? &= !0x0c;
            }
            _ => out.extend(chunk),
        }
        at = end;
    }
    let size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    Some(out)
}

/// The boxes of an MP4 file within `b[from..to]`, each as (type, where it starts, where its content starts, where it ends).
fn boxes(b: &[u8], from: usize, to: usize) -> Option<Vec<([u8; 4], usize, usize, usize)>> {
    let (mut found, mut at) = (vec![], from);
    while at < to {
        let (size, head) = match be32(b, at)? {
            0 => (to - at, 8),
            1 => (u64::from_be_bytes(b.get(at + 8..at + 16)?.try_into().ok()?) as usize, 16),
            size => (size, 8),
        };
        if size < head || at + size > to {
            return None;
        }
        found.push((b.get(at + 4..at + 8)?.try_into().ok()?, at, at + head, at + size));
        at += size;
    }
    Some(found)
}

/// A video's user data and metadata boxes, which hold where it was filmed and with what, become
/// free space of the same size, so that the places the file's index points to stay where they are.
fn mp4(mut b: Vec<u8>) -> Option<Vec<u8>> {
    let mut blank = vec![];
    for (kind, _, inside, end) in boxes(&b, 0, b.len())? {
        if &kind != b"moov" {
            continue;
        }
        for (kind, start, inside, end) in boxes(&b, inside, end)? {
            match &kind {
                b"udta" | b"meta" => blank.push((start, inside, end)),
                b"trak" => blank.extend(boxes(&b, inside, end)?.iter().filter(|x| matches!(&x.0, b"udta" | b"meta")).map(|x| (x.1, x.2, x.3))),
                _ => {}
            }
        }
    }
    for (start, inside, end) in blank {
        b[start + 4..start + 8].copy_from_slice(b"free");
        b[inside..end].fill(0);
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::strip;

    fn has(bytes: &[u8], what: &[u8]) -> bool {
        bytes.windows(what.len()).any(|w| w == what)
    }

    #[test]
    fn a_jpeg_keeps_its_picture_and_which_way_up_it_is() {
        let mut photo = b"\xff\xd8\xff\xe0\0\x06JFIF".to_vec();
        // An Exif block with two entries: the camera held on its side, and its maker.
        photo.extend(b"\xff\xe1\0\x34Exif\0\0MM\0\x2a\0\0\0\x08\0\x02\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\x01\x0f\0\x02\0\0\0\x06\0\0\0\x26\0\0\0\0Secret");
        photo.extend(b"\xff\xfe\0\x08secret");
        photo.extend(b"\xff\xe2\0\x12ICC_PROFILE\0\x01\x01pp");
        let picture = b"\xff\xc0\0\x04\x08\x01\xff\xda\0\x04\x01\x02\x11\xff\0\x22\xff\xd0\x33\xff\xd9";
        photo.extend(picture);
        photo.extend(b"a video the camera put after the picture");
        let sent = strip(photo, false).unwrap();
        assert!(!has(&sent, b"ecret") && !has(&sent, b"a video"));
        assert!(has(&sent, b"JFIF") && has(&sent, b"ICC_PROFILE") && sent.ends_with(picture));
        let exif = sent.windows(6).position(|w| w == b"Exif\0\0").unwrap();
        assert_eq!(crate::signal::exif_orientation(&sent[exif + 6..exif + 32]), Some(6));
        // One that cannot be gone through to its end is not sent as it is.
        assert_eq!(strip(b"\xff\xd8\xff\xe1\0\x40Exif".to_vec(), false), None);
    }

    #[test]
    fn a_png_and_a_webp_lose_their_text_and_exif() {
        let chunk = |kind: &[u8], data: &[u8]| [&(data.len() as u32).to_be_bytes()[..], kind, data, &[0; 4]].concat();
        let png = [b"\x89PNG\r\n\x1a\n".to_vec(), chunk(b"IHDR", &[0; 13]), chunk(b"tEXt", b"Author\0secret"), chunk(b"eXIf", b"secret"), chunk(b"IDAT", b"pixels"), chunk(b"IEND", b""), b"secret".to_vec()].concat();
        let sent = strip(png, false).unwrap();
        assert!(!has(&sent, b"secret") && has(&sent, b"pixels") && sent.ends_with(&chunk(b"IEND", b"")));

        let part = |kind: &[u8], data: &[u8]| [kind, &(data.len() as u32).to_le_bytes()[..], data, &[0][..data.len() & 1]].concat();
        let body = [part(b"VP8X", &[0x0c | 0x10, 0, 0, 0, 1, 0, 0, 1, 0, 0]), part(b"VP8 ", b"pixel"), part(b"EXIF", b"secret"), part(b"XMP ", b"secret!")].concat();
        let webp = [b"RIFF", &(body.len() as u32 + 4).to_le_bytes()[..], b"WEBP", &body].concat();
        let sent = strip(webp, false).unwrap();
        assert!(!has(&sent, b"secret") && has(&sent, b"pixel"));
        assert_eq!(sent[20], 0x10);
        assert_eq!(u32::from_le_bytes(sent[4..8].try_into().unwrap()) as usize, sent.len() - 8);
    }

    #[test]
    fn a_video_keeps_its_length_and_loses_its_user_data() {
        let boxed = |kind: &[u8], data: &[u8]| [&(data.len() as u32 + 8).to_be_bytes()[..], kind, data].concat();
        let trak = boxed(b"trak", &[boxed(b"tkhd", b"track"), boxed(b"meta", b"secret")].concat());
        let moov = boxed(b"moov", &[boxed(b"mvhd", b"movie"), trak, boxed(b"udta", &boxed(b"\xa9xyz", b"+43.6-079.3/"))].concat());
        let file = [boxed(b"ftyp", b"isom"), moov, boxed(b"mdat", b"frames")].concat();
        let sent = strip(file.clone(), true).unwrap();
        assert_eq!(sent.len(), file.len());
        assert!(!has(&sent, b"43.6") && !has(&sent, b"secret") && !has(&sent, b"udta"));
        assert!(has(&sent, b"track") && has(&sent, b"movie") && has(&sent, b"frames"));
        // A sound file of the same form keeps what it says of itself.
        assert_eq!(strip(file.clone(), false), Some(file));
    }
}
