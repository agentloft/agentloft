//! Foldable AVDs: how one folds, read from its `config.ini` (the hinge
//! sensor's `hw.sensor.hinge*` keys, its postures and the cover screen's
//! display region), and how it is folded now, read from the running
//! emulator's physical model (`HINGE_ANGLE0`, `POSTURE`).
//!
//! A Pixel Fold's AVD, for instance:
//!
//! ```text
//! hw.sensor.hinge=yes
//! hw.sensor.hinge.type=1                      # 0 horizontal, 1 vertical
//! hw.sensor.hinge.ranges=0-180
//! hw.sensor.hinge.defaults=180
//! hw.sensor.posture_list=1, 2, 3              # closed, half-opened, opened
//! hw.sensor.hinge_angles_posture_definitions=0-30, 30-150, 150-180
//! hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture=1
//! hw.displayRegion.0.1.width=1080             # the cover screen
//! hw.displayRegion.0.1.height=2424
//! ```
//!
//! Folded to the region's posture, the emulator shows that region (the
//! cover screen) instead of the main display; touches stay in the main
//! display's pixels, which the emulator scales onto it.

use super::grpc::{self, Grpc};
use loft_protocol::{DeviceFold, DevicePosture, FoldAxis};
use std::collections::HashMap;

/// How a foldable AVD folds, as built.
#[derive(Debug, Clone, PartialEq)]
pub struct FoldConfig {
    pub axis: FoldAxis,
    /// The posture at which the emulator switches to the cover screen
    /// (display region 0.1): closed for a fold-in, flipped for a fold-out.
    pub folds_at: DevicePosture,
    /// The cover screen's size in pixels, when it has one.
    pub cover: Option<(u32, u32)>,
    /// The main screen's size in pixels (`hw.lcd`).
    pub main: Option<(u32, u32)>,
    /// The postures it takes and the hinge angles each covers.
    pub postures: Vec<PostureRange>,
    pub hinge_min: f32,
    pub hinge_max: f32,
    /// The hinge's angle when it boots.
    pub hinge_default: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PostureRange {
    pub posture: DevicePosture,
    pub from: f32,
    pub to: f32,
}

/// Android's posture number (`Posture.PostureValue`, `hw.sensor.posture_list`).
pub fn posture_from_android(n: i32) -> Option<DevicePosture> {
    Some(match n {
        grpc::posture::CLOSED => DevicePosture::Closed,
        grpc::posture::HALF_OPENED => DevicePosture::HalfOpened,
        grpc::posture::OPENED => DevicePosture::Opened,
        grpc::posture::FLIPPED => DevicePosture::Flipped,
        grpc::posture::TENT => DevicePosture::Tent,
        _ => return None,
    })
}

pub fn posture_to_android(posture: DevicePosture) -> i32 {
    match posture {
        DevicePosture::Closed => grpc::posture::CLOSED,
        DevicePosture::HalfOpened => grpc::posture::HALF_OPENED,
        DevicePosture::Opened => grpc::posture::OPENED,
        DevicePosture::Flipped => grpc::posture::FLIPPED,
        DevicePosture::Tent => grpc::posture::TENT,
    }
}

/// `0-180`, `30-150`, or `0-30-10` (with a default angle, ignored here).
fn range(text: &str) -> Option<(f32, f32)> {
    let mut parts = text.trim().split('-').map(|p| p.trim().parse::<f32>());
    let from = parts.next()?.ok()?;
    let to = parts.next()?.ok()?;
    Some((from, to))
}

impl FoldConfig {
    /// The fold of an AVD with these `config.ini` keys; `None` when it has
    /// no hinge.
    pub fn parse(config: &HashMap<String, String>) -> Option<Self> {
        let get = |key: &str| config.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
        let yes = |key: &str| get(key).is_some_and(|v| v == "yes" || v == "true");
        if !yes("hw.sensor.hinge") {
            return None;
        }
        let num = |key: &str| get(key).and_then(|v| v.parse::<f32>().ok());
        if num("hw.sensor.hinge.count").is_some_and(|n| n < 1.0) {
            return None;
        }
        // The first hinge's: these lists hold one entry per hinge.
        let first = |key: &str| get(key).and_then(|v| v.split(',').next().map(str::to_owned));
        let (hinge_min, hinge_max) = first("hw.sensor.hinge.ranges")
            .and_then(|r| range(&r))
            .unwrap_or((0.0, 180.0));
        let hinge_default = first("hw.sensor.hinge.defaults")
            .and_then(|d| d.trim().parse().ok())
            .unwrap_or(hinge_max)
            .clamp(hinge_min, hinge_max);
        let numbers: Vec<i32> = get("hw.sensor.posture_list")
            .unwrap_or_default()
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        let ranges: Vec<(f32, f32)> = get("hw.sensor.hinge_angles_posture_definitions")
            .unwrap_or_default()
            .split(',')
            .filter_map(range)
            .collect();
        let postures: Vec<PostureRange> = numbers
            .iter()
            .zip(ranges.iter().map(Some).chain(std::iter::repeat(None)))
            .filter_map(|(&n, r)| {
                let posture = posture_from_android(n)?;
                let (from, to) = r.copied().unwrap_or((hinge_min, hinge_max));
                Some(PostureRange { posture, from, to })
            })
            .collect();
        let folds_at = num("hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture")
            .and_then(|n| posture_from_android(n as i32))
            .unwrap_or(DevicePosture::Closed);
        let cover = match (
            num("hw.displayRegion.0.1.width"),
            num("hw.displayRegion.0.1.height"),
        ) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((w as u32, h as u32)),
            _ => None,
        };
        let main = match (num("hw.lcd.width"), num("hw.lcd.height")) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((w as u32, h as u32)),
            _ => None,
        };
        let axis = if get("hw.sensor.hinge.type") == Some("0") {
            FoldAxis::Horizontal
        } else {
            FoldAxis::Vertical
        };
        Some(Self {
            axis,
            main,
            folds_at,
            cover,
            postures,
            hinge_min,
            hinge_max,
            hinge_default,
        })
    }

    /// The main screen is inside when folded (it folds shut, not back).
    pub fn inward(&self) -> bool {
        self.folds_at == DevicePosture::Closed
    }

    pub fn takes(&self, posture: DevicePosture) -> bool {
        self.postures.iter().any(|p| p.posture == posture)
    }

    /// The posture the hinge at `degrees` puts it in (ranges meet at their
    /// ends: the later one wins, as the emulator's do).
    pub fn posture_at(&self, degrees: f32) -> Option<DevicePosture> {
        self.postures
            .iter()
            .rev()
            .find(|p| degrees >= p.from && degrees <= p.to)
            .or_else(|| {
                // Outside every range: the nearest.
                self.postures.iter().min_by(|a, b| {
                    let d = |p: &PostureRange| (p.from - degrees).abs().min((p.to - degrees).abs());
                    d(a).total_cmp(&d(b))
                })
            })
            .map(|p| p.posture)
    }

    /// The pictures show the cover screen in `posture`.
    pub fn on_cover(&self, posture: DevicePosture) -> bool {
        self.cover.is_some() && posture == self.folds_at
    }

    /// The screen shown in `posture`, upright, in pixels: the cover
    /// screen's when folded to it, else the main display's (`main`).
    pub fn screen(&self, posture: DevicePosture, main: (u32, u32)) -> (u32, u32) {
        match self.cover {
            Some(cover) if self.on_cover(posture) => cover,
            _ => main,
        }
    }

    /// As the protocol says it, folded `posture` at `hinge` degrees.
    pub fn state(&self, posture: DevicePosture, hinge: f32) -> DeviceFold {
        DeviceFold {
            axis: self.axis,
            inward: self.inward(),
            cover_screen: self.cover.is_some(),
            postures: self.postures.iter().map(|p| p.posture).collect(),
            posture,
            hinge,
            hinge_min: self.hinge_min,
            hinge_max: self.hinge_max,
            on_cover: self.on_cover(posture),
            main_size: self.main,
            cover_size: self.cover,
        }
    }

    /// As it is when it boots.
    pub fn at_rest(&self) -> DeviceFold {
        let posture = self
            .posture_at(self.hinge_default)
            .unwrap_or(DevicePosture::Opened);
        self.state(posture, self.hinge_default)
    }
}

/// One value of the emulator's physical model.
async fn physical(conn: &Grpc, target: i32) -> anyhow::Result<f32> {
    let model: grpc::PhysicalModelValue = conn
        .unary(
            "getPhysicalModel",
            &grpc::PhysicalModelValue {
                target,
                ..Default::default()
            },
        )
        .await?;
    model
        .value
        .and_then(|v| v.data.first().copied())
        .ok_or_else(|| anyhow::anyhow!("the emulator didn't say its physical model {target}"))
}

/// How a running foldable is folded now: its hinge, and the posture the
/// emulator says (or the one its hinge puts it in).
pub async fn now(conn: &Grpc, config: &FoldConfig) -> anyhow::Result<DeviceFold> {
    let hinge = physical(conn, grpc::physical::HINGE_ANGLE0).await?;
    let posture = match physical(conn, grpc::physical::POSTURE).await {
        Ok(n) => posture_from_android(n.round() as i32),
        Err(_) => None,
    }
    .or_else(|| config.posture_at(hinge))
    .unwrap_or(DevicePosture::Opened);
    Ok(config.state(posture, hinge))
}

/// Put the hinge at `degrees`.
pub async fn set_hinge(conn: &Grpc, degrees: f32) -> anyhow::Result<()> {
    conn.unary::<_, grpc::Empty>(
        "setPhysicalModel",
        &grpc::PhysicalModelValue {
            target: grpc::physical::HINGE_ANGLE0,
            status: 0,
            value: Some(grpc::ParameterValue {
                data: vec![degrees],
            }),
            interpolation: 1,
        },
    )
    .await?;
    Ok(())
}

pub async fn set_posture(conn: &Grpc, posture: DevicePosture) -> anyhow::Result<()> {
    conn.unary::<_, grpc::Empty>(
        "setPosture",
        &grpc::Posture {
            value: posture_to_android(posture),
        },
    )
    .await?;
    Ok(())
}

/// Wait (briefly) until the emulator says `posture`: it applies a change on
/// its own loop, after answering.
pub async fn settle(conn: &Grpc, posture: DevicePosture) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(n) = physical(conn, grpc::physical::POSTURE).await
            && posture_from_android(n.round() as i32) == Some(posture)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::android::sdk::parse_ini;

    const PIXEL_FOLD: &str = "\
hw.device.name=pixel_9_pro_fold
hw.displayRegion.0.1.height=2424
hw.displayRegion.0.1.width=1080
hw.displayRegion.0.1.xOffset=0
hw.displayRegion.0.1.yOffset=0
hw.displayRegion.0.2.height=0
hw.displayRegion.0.2.width=0
hw.lcd.height=2152
hw.lcd.width=2076
hw.sensor.hinge=yes
hw.sensor.hinge.areas=1038-0-0-2152
hw.sensor.hinge.count=1
hw.sensor.hinge.defaults=180
hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture=1
hw.sensor.hinge.ranges=0-180
hw.sensor.hinge.sub_type=1
hw.sensor.hinge.type=1
hw.sensor.hinge_angles_posture_definitions=0-30, 30-150, 150-180
hw.sensor.posture_list=1, 2, 3
";

    #[test]
    fn reads_a_pixel_fold() {
        let fold = FoldConfig::parse(&parse_ini(PIXEL_FOLD)).unwrap();
        assert_eq!(fold.axis, FoldAxis::Vertical);
        assert!(fold.inward());
        assert_eq!(fold.cover, Some((1080, 2424)));
        assert_eq!((fold.hinge_min, fold.hinge_max), (0.0, 180.0));
        assert_eq!(
            fold.postures,
            vec![
                PostureRange {
                    posture: DevicePosture::Closed,
                    from: 0.0,
                    to: 30.0
                },
                PostureRange {
                    posture: DevicePosture::HalfOpened,
                    from: 30.0,
                    to: 150.0
                },
                PostureRange {
                    posture: DevicePosture::Opened,
                    from: 150.0,
                    to: 180.0
                },
            ]
        );
        assert_eq!(fold.posture_at(0.0), Some(DevicePosture::Closed));
        assert_eq!(fold.posture_at(10.0), Some(DevicePosture::Closed));
        assert_eq!(fold.posture_at(30.0), Some(DevicePosture::HalfOpened));
        assert_eq!(fold.posture_at(90.0), Some(DevicePosture::HalfOpened));
        assert_eq!(fold.posture_at(180.0), Some(DevicePosture::Opened));
        let rest = fold.at_rest();
        assert_eq!(rest.posture, DevicePosture::Opened);
        assert_eq!(rest.hinge, 180.0);
        assert!(!rest.on_cover);
        assert!(rest.cover_screen);
        let shut = fold.state(DevicePosture::Closed, 0.0);
        assert!(shut.on_cover);
        assert_eq!(
            fold.screen(DevicePosture::Closed, (2076, 2152)),
            (1080, 2424)
        );
        assert_eq!(
            fold.screen(DevicePosture::HalfOpened, (2076, 2152)),
            (2076, 2152)
        );
    }

    #[test]
    fn reads_a_flip_and_a_fold_out() {
        // "6.7in Foldable": a flip phone without a cover screen.
        let flip = FoldConfig::parse(&parse_ini(
            "hw.sensor.hinge=yes\nhw.sensor.hinge.count=1\nhw.sensor.hinge.type=0\n\
             hw.sensor.hinge.ranges=0-180\nhw.sensor.hinge.defaults=180\n\
             hw.sensor.posture_list=1, 2, 3\n\
             hw.sensor.hinge_angles_posture_definitions=0-30, 30-150, 150-180\n\
             hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture=1\n\
             hw.displayRegion.0.1.width=0\nhw.displayRegion.0.1.height=0\n",
        ))
        .unwrap();
        assert_eq!(flip.axis, FoldAxis::Horizontal);
        assert!(flip.inward());
        assert_eq!(flip.cover, None);
        assert!(!flip.state(DevicePosture::Closed, 0.0).on_cover);

        // "8in Foldable": folds outward, its screen's left part showing.
        let out = FoldConfig::parse(&parse_ini(
            "hw.sensor.hinge=yes\nhw.sensor.hinge.count=1\nhw.sensor.hinge.type=1\n\
             hw.sensor.hinge.ranges=180-360\nhw.sensor.hinge.defaults=180\n\
             hw.sensor.posture_list=3, 4\n\
             hw.sensor.hinge_angles_posture_definitions=180-330, 330-360\n\
             hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture=4\n\
             hw.displayRegion.0.1.width=1148\nhw.displayRegion.0.1.height=2480\n",
        ))
        .unwrap();
        assert!(!out.inward());
        assert_eq!(out.folds_at, DevicePosture::Flipped);
        assert_eq!((out.hinge_min, out.hinge_max), (180.0, 360.0));
        assert_eq!(out.posture_at(350.0), Some(DevicePosture::Flipped));
        assert!(out.takes(DevicePosture::Flipped));
        assert!(!out.takes(DevicePosture::Closed));
        assert!(out.state(DevicePosture::Flipped, 360.0).on_cover);
        assert_eq!(out.at_rest().posture, DevicePosture::Opened);
    }

    #[test]
    fn a_phone_has_no_fold() {
        assert_eq!(
            FoldConfig::parse(&parse_ini("hw.lcd.width=1080\nhw.sensor.hinge=no\n")),
            None
        );
        assert_eq!(
            FoldConfig::parse(&parse_ini("hw.sensor.hinge=yes\nhw.sensor.hinge.count=0\n")),
            None
        );
    }

    #[test]
    fn maps_postures_both_ways() {
        for p in [
            DevicePosture::Closed,
            DevicePosture::HalfOpened,
            DevicePosture::Opened,
            DevicePosture::Flipped,
            DevicePosture::Tent,
        ] {
            assert_eq!(posture_from_android(posture_to_android(p)), Some(p));
        }
        assert_eq!(posture_from_android(0), None);
        assert_eq!(posture_from_android(6), None);
    }
}
