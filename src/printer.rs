//! Zebra label printer (ZD411) over the network: ZPL is sent raw to TCP port
//! 9100, and the printer's status is read with `~HS`.

use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use image::GrayImage;

pub const DEFAULT_PORT: u16 = 9100;
const TIMEOUT: Duration = Duration::from_secs(4);

/// Where the printer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Printer {
    pub host: String,
    pub port: u16,
}

/// What `~HS` reports, reduced to what matters before printing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub paper_out: bool,
    pub paused: bool,
    pub head_open: bool,
    pub ribbon_out: bool,
    pub thermal_transfer: bool,
    /// Label length in dots as configured in the printer.
    pub label_length: u32,
}

impl Status {
    /// Parses the answer to `~HS`: three lines of comma separated fields.
    pub fn parse(response: &str) -> Result<Self> {
        let lines: Vec<Vec<&str>> = response
            .split(['\u{2}', '\u{3}'])
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.split(',').map(str::trim).collect())
            .collect();
        let (first, second) = match (lines.first(), lines.get(1)) {
            (Some(a), Some(b)) if a.len() >= 5 && b.len() >= 5 => (a, b),
            _ => bail!("unexpected printer status: {response:?}"),
        };
        let flag = |fields: &Vec<&str>, i: usize| fields.get(i).is_some_and(|f| *f == "1");
        Ok(Self {
            paper_out: flag(first, 1),
            paused: flag(first, 2),
            label_length: first[3].parse().context("label length in printer status")?,
            head_open: flag(second, 2),
            ribbon_out: flag(second, 3),
            thermal_transfer: flag(second, 4),
        })
    }

    /// Why printing would fail right now, if it would.
    pub fn problem(&self) -> Option<&'static str> {
        if self.head_open {
            Some("the print head is open")
        } else if self.paper_out {
            Some("the printer is out of labels")
        } else if self.thermal_transfer && self.ribbon_out {
            Some("the printer is out of ribbon")
        } else if self.paused {
            Some("the printer is paused")
        } else {
            None
        }
    }
}

impl Printer {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.trim().to_string(),
            port,
        }
    }

    fn connect(&self) -> Result<TcpStream> {
        let addr = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .with_context(|| format!("resolve printer {}", self.host))?
            .next()
            .with_context(|| format!("no address for printer {}", self.host))?;
        let stream = TcpStream::connect_timeout(&addr, TIMEOUT)
            .with_context(|| format!("connect to printer {}:{}", self.host, self.port))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(stream)
    }

    /// Asks the printer for its status (does not print anything).
    pub fn status(&self) -> Result<Status> {
        let mut stream = self.connect()?;
        stream.write_all(b"~HS")?;
        // the answer is three STX...ETX framed lines
        let mut response = Vec::new();
        let mut buf = [0u8; 256];
        while response.iter().filter(|b| **b == 3).count() < 3 {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(e).context("read printer status"),
            }
        }
        Status::parse(&String::from_utf8_lossy(&response))
    }

    /// Sends ZPL to the printer.
    pub fn send(&self, zpl: &str) -> Result<()> {
        let mut stream = self.connect()?;
        stream
            .write_all(zpl.as_bytes())
            .context("send to printer")?;
        stream.flush()?;
        Ok(())
    }

    /// Checks the status, then prints. Refuses if the printer reports a problem.
    pub fn print(&self, zpl: &str) -> Result<()> {
        let status = self.status()?;
        if let Some(problem) = status.problem() {
            bail!("not printing: {problem}");
        }
        self.send(zpl)
    }
}

/// ZPL that prints `img` (black pixels are printed) with its top left corner
/// at `(x, y)` dots from the label origin. The printer's stored settings
/// (darkness, speed, media, label length) are left alone.
pub fn bitmap_zpl(img: &GrayImage, x: u32, y: u32) -> String {
    let bytes_per_row = img.width().div_ceil(8) as usize;
    let mut data = Vec::with_capacity(bytes_per_row * img.height() as usize);
    for row in 0..img.height() {
        for byte in 0..bytes_per_row {
            let mut value = 0u8;
            for bit in 0..8 {
                let col = byte as u32 * 8 + bit;
                if col < img.width() && img.get_pixel(col, row).0[0] < 128 {
                    value |= 0x80 >> bit;
                }
            }
            data.push(value);
        }
    }
    let hex: String = data.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "^XA\n^CI28\n^LH0,0\n^FO{x},{y}^GFA,{n},{n},{bytes_per_row},{hex}^FS\n^PQ1\n^XZ\n",
        n = data.len()
    )
}

/// A label for finding out how the stock sits in the printer: a frame around
/// the whole print area with a millimetre ruler along the top and the left
/// edge (numbers every 5 mm).
pub fn calibration_zpl(width: u32, length: u32) -> String {
    let mut zpl = String::from("^XA\n^CI28\n^LH0,0\n");
    zpl.push_str(&format!("^FO0,0^GB{width},{length},1^FS\n"));
    let mm = 8; // dots per mm at 203 dpi
    for i in 0..=(width / mm) {
        let x = i * mm;
        let tick = if i % 5 == 0 { 14 } else { 6 };
        zpl.push_str(&format!("^FO{x},1^GB1,{tick},1^FS\n"));
        if i % 5 == 0 && i > 0 && x + 20 < width {
            zpl.push_str(&format!(
                "^FO{},18^A0N,16,16^FD{i}^FS\n",
                x.saturating_sub(5)
            ));
        }
    }
    for i in 0..=(length / mm) {
        let y = i * mm;
        let tick = if i % 5 == 0 { 14 } else { 6 };
        zpl.push_str(&format!("^FO1,{y}^GB{tick},1,1^FS\n"));
        if i % 5 == 0 && i > 0 && y + 20 < length {
            zpl.push_str(&format!(
                "^FO18,{}^A0N,16,16^FD{i}^FS\n",
                y.saturating_sub(7)
            ));
        }
    }
    zpl.push_str(&format!(
        "^FO60,{}^A0N,22,22^FDmm from the print origin^FS\n^FO60,{}^A0N,22,22^FD{width}x{length} dots^FS\n",
        length / 2 - 14,
        length / 2 + 14
    ));
    zpl.push_str("^PQ1\n^XZ\n");
    zpl
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;
    use std::net::TcpListener;
    use std::thread;

    // as answered by the real ZD411
    const STATUS_OK: &str = "\u{2}030,0,0,0310,000,0,0,0,000,0,0,0\u{3}\r\n\u{2}001,0,0,0,1,2,3,0,00000000,1,000\u{3}\r\n\u{2}0000,0\u{3}\r\n";

    #[test]
    fn parses_the_real_status_answer() {
        let s = Status::parse(STATUS_OK).unwrap();
        assert_eq!(
            s,
            Status {
                paper_out: false,
                paused: false,
                head_open: false,
                ribbon_out: false,
                thermal_transfer: true,
                label_length: 310,
            }
        );
        assert_eq!(s.problem(), None);
    }

    #[test]
    fn problems_are_reported_in_a_useful_order() {
        let base = Status::parse(STATUS_OK).unwrap();
        let with = |f: fn(&mut Status)| {
            let mut s = base.clone();
            f(&mut s);
            s.problem()
        };
        assert_eq!(with(|s| s.head_open = true), Some("the print head is open"));
        assert_eq!(
            with(|s| s.paper_out = true),
            Some("the printer is out of labels")
        );
        assert_eq!(
            with(|s| s.ribbon_out = true),
            Some("the printer is out of ribbon")
        );
        assert_eq!(with(|s| s.paused = true), Some("the printer is paused"));
        // direct thermal printing has no ribbon
        assert_eq!(
            with(|s| {
                s.ribbon_out = true;
                s.thermal_transfer = false;
            }),
            None
        );
    }

    #[test]
    fn flags_come_from_the_right_fields() {
        let s = Status::parse(
            "\u{2}030,1,1,0310,000,0,0,0,000,0,0,0\u{3}\r\n\u{2}001,0,1,1,0,2,3,0,00000000,1,000\u{3}\r\n\u{2}0000,0\u{3}\r\n",
        )
        .unwrap();
        assert!(s.paper_out && s.paused && s.head_open && s.ribbon_out);
        assert!(!s.thermal_transfer);
    }

    #[test]
    fn garbage_status_is_an_error() {
        assert!(Status::parse("").is_err());
        assert!(Status::parse("hello").is_err());
        assert!(Status::parse("\u{2}1,2\u{3}\u{2}3\u{3}").is_err());
        assert!(Status::parse("\u{2}030,0,0,abc,0\u{3}\u{2}001,0,0,0,1\u{3}").is_err());
    }

    #[test]
    fn bitmap_becomes_hex_rows_with_black_as_one() {
        let mut img = GrayImage::from_pixel(10, 2, Luma([255]));
        img.put_pixel(0, 0, Luma([0])); // first bit of the first byte
        img.put_pixel(9, 1, Luma([0])); // bit 1 of the second byte in row 2
        let zpl = bitmap_zpl(&img, 3, 4);
        // 10 pixels per row -> 2 bytes per row, 4 bytes in total
        assert!(zpl.contains("^FO3,4^GFA,4,4,2,80000040"), "{zpl}");
        assert!(zpl.starts_with("^XA\n"));
        assert!(zpl.trim_end().ends_with("^XZ"));
        assert!(zpl.contains("^PQ1"));
    }

    #[test]
    fn a_full_label_has_matching_sizes() {
        let img = GrayImage::from_pixel(408, 304, Luma([0]));
        let zpl = bitmap_zpl(&img, 0, 0);
        let total = 51 * 304;
        assert!(zpl.contains(&format!("^GFA,{total},{total},51,")));
        let hex_len = zpl
            .split("^GFA,")
            .nth(1)
            .unwrap()
            .split(',')
            .nth(3)
            .unwrap()
            .split("^FS")
            .next()
            .unwrap()
            .len();
        assert_eq!(hex_len, total * 2);
        assert!(zpl.contains("FFFFFFFF"), "all black");
    }

    #[test]
    fn zpl_does_not_change_the_printer_settings() {
        // no darkness, speed, media type, print width or label length commands
        let zpl = bitmap_zpl(&GrayImage::from_pixel(8, 8, Luma([255])), 0, 0);
        for forbidden in [
            "^MD", "~SD", "^PR", "^MN", "^MT", "^PW", "^LL", "~JC", "^JU", "^PM",
        ] {
            assert!(!zpl.contains(forbidden), "{forbidden} in {zpl}");
        }
        let cal = calibration_zpl(448, 310);
        for forbidden in [
            "^MD", "~SD", "^PR", "^MN", "^MT", "^PW", "^LL", "~JC", "^JU",
        ] {
            assert!(!cal.contains(forbidden), "{forbidden} in {cal}");
        }
    }

    #[test]
    fn calibration_label_has_frame_ruler_and_numbers() {
        let zpl = calibration_zpl(448, 310);
        assert!(zpl.contains("^FO0,0^GB448,310,1^FS"));
        assert!(zpl.contains("^FD5^FS") && zpl.contains("^FD50^FS"));
        assert!(zpl.contains("448x310 dots"));
        assert!(zpl.starts_with("^XA") && zpl.trim_end().ends_with("^XZ"));
    }

    /// A fake printer: answers `~HS`, records anything else.
    fn fake_printer(status: &'static str) -> (Printer, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut received = Vec::new();
            // connections: status query (and print job if the status was fine)
            listener.set_nonblocking(false).unwrap();
            for _ in 0..2 {
                let (mut stream, _) = match listener.accept() {
                    Ok(c) => c,
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(600)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            data.extend_from_slice(&buf[..n]);
                            if data.starts_with(b"~HS") {
                                stream.write_all(status.as_bytes()).unwrap();
                                break;
                            }
                        }
                    }
                }
                received.push(String::from_utf8_lossy(&data).to_string());
            }
            received
        });
        (Printer::new("127.0.0.1", port), handle)
    }

    #[test]
    fn reads_status_over_tcp() {
        let (printer, server) = fake_printer(STATUS_OK);
        let status = printer.status().unwrap();
        assert!(status.thermal_transfer);
        // unblock the fake's second accept
        let _ = TcpStream::connect((printer.host.as_str(), printer.port));
        assert_eq!(server.join().unwrap()[0], "~HS");
    }

    #[test]
    fn prints_when_the_status_is_fine() {
        let (printer, server) = fake_printer(STATUS_OK);
        printer.print("^XA^FDtest^FS^XZ").unwrap();
        let received = server.join().unwrap();
        assert_eq!(received[0], "~HS");
        assert_eq!(received[1], "^XA^FDtest^FS^XZ");
    }

    #[test]
    fn refuses_to_print_when_the_printer_has_a_problem() {
        let out_of_labels = "\u{2}030,1,0,0310,000,0,0,0,000,0,0,0\u{3}\r\n\u{2}001,0,0,0,1,2,3,0,00000000,1,000\u{3}\r\n\u{2}0000,0\u{3}\r\n";
        let (printer, server) = fake_printer(out_of_labels);
        let err = printer.print("^XA^XZ").unwrap_err().to_string();
        assert!(err.contains("out of labels"), "{err}");
        let _ = TcpStream::connect((printer.host.as_str(), printer.port));
        let received = server.join().unwrap();
        assert_eq!(received[0], "~HS");
        assert!(
            !received.iter().any(|r| r.contains("^XA")),
            "nothing may be sent: {received:?}"
        );
    }

    #[test]
    fn an_unreachable_printer_is_a_clear_error() {
        let free_port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = format!(
            "{:#}",
            Printer::new("127.0.0.1", free_port).status().unwrap_err()
        );
        assert!(err.contains("connect to printer"), "{err}");
    }
}
