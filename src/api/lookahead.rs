use crate::api::internal::InterConfig;
use crate::config::EncoderConfig;
use crate::context::{BlockOffset, FrameBlocks, TileBlockOffset};
use crate::cpu_features::CpuFeatureLevel;
use crate::dist::get_satd;
use crate::encoder::{
  FrameInvariants, FrameState, Sequence, IMPORTANCE_BLOCK_SIZE,
};
use crate::frame::{AsRegion, PlaneOffset};
use crate::me::estimate_tile_motion;
use crate::partition::{get_intra_edges, BlockSize};
use crate::predict::{IntraParam, PredictionMode};
use crate::rayon::iter::*;
use crate::tiling::{Area, TileRect};
use crate::transform::TxSize;
use crate::{Frame, Pixel};
use rust_hawktracer::*;
use std::sync::Arc;
use v_frame::pixel::CastFromPrimitive;

pub(crate) const IMP_BLOCK_MV_UNITS_PER_PIXEL: i64 = 8;
pub(crate) const IMP_BLOCK_SIZE_IN_MV_UNITS: i64 =
  IMPORTANCE_BLOCK_SIZE as i64 * IMP_BLOCK_MV_UNITS_PER_PIXEL;
pub(crate) const IMP_BLOCK_AREA_IN_MV_UNITS: i64 =
  IMP_BLOCK_SIZE_IN_MV_UNITS * IMP_BLOCK_SIZE_IN_MV_UNITS;

#[hawktracer(estimate_intra_costs)]
pub(crate) fn estimate_intra_costs<T: Pixel>(
  frame: &Frame<T>, bit_depth: usize, cpu_feature_level: CpuFeatureLevel,
) -> Box<[u32]> {
  let plane = &frame.planes[0];
  let mut plane_after_prediction = frame.planes[0].clone();

  let bsize = BlockSize::from_width_and_height(
    IMPORTANCE_BLOCK_SIZE,
    IMPORTANCE_BLOCK_SIZE,
  );
  let tx_size = bsize.tx_size();

  let h_in_imp_b = plane.cfg.height / IMPORTANCE_BLOCK_SIZE;
  let w_in_imp_b = plane.cfg.width / IMPORTANCE_BLOCK_SIZE;
  let mut intra_costs = Vec::with_capacity(h_in_imp_b * w_in_imp_b);

  for y in 0..h_in_imp_b {
    for x in 0..w_in_imp_b {
      let plane_org = plane.region(Area::Rect {
        x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
        y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
        width: IMPORTANCE_BLOCK_SIZE,
        height: IMPORTANCE_BLOCK_SIZE,
      });

      // TODO: other intra prediction modes.
      let edge_buf = get_intra_edges(
        &plane.as_region(),
        TileBlockOffset(BlockOffset { x, y }),
        0,
        0,
        bsize,
        PlaneOffset {
          x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
          y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
        },
        TxSize::TX_8X8,
        bit_depth,
        Some(PredictionMode::DC_PRED),
        false,
        IntraParam::None,
      );

      let mut plane_after_prediction_region = plane_after_prediction
        .region_mut(Area::Rect {
          x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
          y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
          width: IMPORTANCE_BLOCK_SIZE,
          height: IMPORTANCE_BLOCK_SIZE,
        });

      PredictionMode::DC_PRED.predict_intra(
        TileRect {
          x: x * IMPORTANCE_BLOCK_SIZE,
          y: y * IMPORTANCE_BLOCK_SIZE,
          width: IMPORTANCE_BLOCK_SIZE,
          height: IMPORTANCE_BLOCK_SIZE,
        },
        &mut plane_after_prediction_region,
        tx_size,
        bit_depth,
        &[], // Not used by DC_PRED
        IntraParam::None,
        None, // Not used by DC_PRED
        &edge_buf,
        cpu_feature_level,
      );

      let plane_after_prediction_region =
        plane_after_prediction.region(Area::Rect {
          x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
          y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
          width: IMPORTANCE_BLOCK_SIZE,
          height: IMPORTANCE_BLOCK_SIZE,
        });

      let intra_cost = get_satd(
        &plane_org,
        &plane_after_prediction_region,
        bsize,
        bit_depth,
        cpu_feature_level,
      );

      intra_costs.push(intra_cost);
    }
  }

  intra_costs.into_boxed_slice()
}

#[hawktracer(estimate_inter_costs_histogram)]
pub(crate) fn estimate_inter_costs_histogram_blocks<T: Pixel>(
  frame: Arc<Frame<T>>, ref_frame: Arc<Frame<T>>,
) -> Box<[u32]> {
  let plane_org = &frame.planes[0];
  let plane_ref = &ref_frame.planes[0];
  let h_in_imp_b = plane_org.cfg.height / IMPORTANCE_BLOCK_SIZE;
  let w_in_imp_b = plane_org.cfg.width / IMPORTANCE_BLOCK_SIZE;
  let mut inter_costs = Vec::with_capacity(h_in_imp_b * w_in_imp_b);

  (0..h_in_imp_b).for_each(|y| {
    (0..w_in_imp_b).for_each(|x| {
      // Coordinates of the top-left corner of the reference block, in MV
      // units.
      let region_org = plane_org.region(Area::Rect {
        x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
        y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
        width: IMPORTANCE_BLOCK_SIZE,
        height: IMPORTANCE_BLOCK_SIZE,
      });

      let region_ref = plane_ref.region(Area::Rect {
        x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
        y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
        width: IMPORTANCE_BLOCK_SIZE,
        height: IMPORTANCE_BLOCK_SIZE,
      });

      let mut count = 0i64;
      let mut histogram_org_sum = 0i64;
      let iter_org = region_org.rows_iter();
      for row in iter_org {
        for pixel in row {
          let cur = u16::cast_from(*pixel);
          histogram_org_sum += cur as i64;
          count += 1;
        }
      }

      let mut histogram_ref_sum = 0i64;
      let iter_ref = region_ref.rows_iter();
      for row in iter_ref {
        for pixel in row {
          let cur = u16::cast_from(*pixel);
          histogram_ref_sum += cur as i64;
        }
      }

      let mean = (((histogram_org_sum + count / 2) / count)
        - ((histogram_ref_sum + count / 2) / count))
        .abs();

      inter_costs.push(mean as u32);
    });
  });
  inter_costs.into_boxed_slice()
}

#[hawktracer(estimate_inter_costs)]
pub(crate) fn estimate_inter_costs<T: Pixel>(
  frame: Arc<Frame<T>>, ref_frame: Arc<Frame<T>>, bit_depth: usize,
  mut config: EncoderConfig, sequence: Arc<Sequence>,
) -> Box<[u32]> {
  config.low_latency = true;
  config.speed_settings.multiref = false;
  let inter_cfg = InterConfig::new(&config);
  let last_fi = FrameInvariants::new_key_frame(Arc::new(config), sequence, 0);
  let mut fi =
    FrameInvariants::new_inter_frame(&last_fi, &inter_cfg, 0, 1, 2, false);

  // Compute the motion vectors.
  let mut fs = FrameState::new_with_frame(&fi, frame.clone());
  compute_motion_vectors(&mut fi, &mut fs, &inter_cfg);

  // Estimate inter costs
  let plane_org = &frame.planes[0];
  let plane_ref = &ref_frame.planes[0];
  let h_in_imp_b = plane_org.cfg.height / IMPORTANCE_BLOCK_SIZE;
  let w_in_imp_b = plane_org.cfg.width / IMPORTANCE_BLOCK_SIZE;
  let mut inter_costs = Vec::with_capacity(h_in_imp_b * w_in_imp_b);
  let stats = &fs.frame_me_stats[0];
  let bsize = BlockSize::from_width_and_height(
    IMPORTANCE_BLOCK_SIZE,
    IMPORTANCE_BLOCK_SIZE,
  );
  (0..h_in_imp_b).for_each(|y| {
    (0..w_in_imp_b).for_each(|x| {
      let mv = stats[y * 2][x * 2].mv;

      // Coordinates of the top-left corner of the reference block, in MV
      // units.
      let reference_x = x as i64 * IMP_BLOCK_SIZE_IN_MV_UNITS + mv.col as i64;
      let reference_y = y as i64 * IMP_BLOCK_SIZE_IN_MV_UNITS + mv.row as i64;

      let region_org = plane_org.region(Area::Rect {
        x: (x * IMPORTANCE_BLOCK_SIZE) as isize,
        y: (y * IMPORTANCE_BLOCK_SIZE) as isize,
        width: IMPORTANCE_BLOCK_SIZE,
        height: IMPORTANCE_BLOCK_SIZE,
      });

      let region_ref = plane_ref.region(Area::Rect {
        x: reference_x as isize / IMP_BLOCK_MV_UNITS_PER_PIXEL as isize,
        y: reference_y as isize / IMP_BLOCK_MV_UNITS_PER_PIXEL as isize,
        width: IMPORTANCE_BLOCK_SIZE,
        height: IMPORTANCE_BLOCK_SIZE,
      });

      inter_costs.push(get_satd(
        &region_org,
        &region_ref,
        bsize,
        bit_depth,
        fi.cpu_feature_level,
      ));
    });
  });
  inter_costs.into_boxed_slice()
}

#[hawktracer(compute_motion_vectors)]
pub(crate) fn compute_motion_vectors<T: Pixel>(
  fi: &mut FrameInvariants<T>, fs: &mut FrameState<T>, inter_cfg: &InterConfig,
) {
  let mut blocks = FrameBlocks::new(fi.w_in_b, fi.h_in_b);
  fi.sequence
    .tiling
    .tile_iter_mut(fs, &mut blocks)
    .collect::<Vec<_>>()
    .into_par_iter()
    .for_each(|mut ctx| {
      let ts = &mut ctx.ts;
      estimate_tile_motion(fi, ts, inter_cfg);
    });
}
