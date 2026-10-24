//! From a model file's space into Unity's: Unity's X points the other way
//! (a left-handed space), and lengths take the import's scale.
pub struct Space {
    scale: f32,
}

impl Space {
    pub fn new(scale: f32) -> Self {
        Self { scale }
    }

    /// Whether the conversion mirrors, which turns triangles around.
    pub fn mirrored(&self) -> bool {
        true
    }

    pub fn position(&self, p: [f32; 3]) -> [f32; 3] {
        [-p[0] * self.scale, p[1] * self.scale, p[2] * self.scale]
    }

    pub fn direction(&self, v: [f32; 3]) -> [f32; 3] {
        [-v[0], v[1], v[2]]
    }

    /// A rotation `(x, y, z, w)` of the file as Unity holds it.
    pub fn rotation(&self, q: [f32; 4]) -> [f32; 4] {
        [q[0], -q[1], -q[2], q[3]]
    }

    /// An affine matrix of the file, rows by columns, as Unity holds it:
    /// mirrored on both sides and its translation scaled.
    pub fn matrix(&self, m: &ufbx::Matrix) -> [[f32; 4]; 4] {
        let rows = [
            [m.m00, m.m01, m.m02, m.m03],
            [m.m10, m.m11, m.m12, m.m13],
            [m.m20, m.m21, m.m22, m.m23],
        ];
        let mut out = [[0f32; 4]; 4];
        let sign = |i: usize| if i == 0 { -1.0 } else { 1.0 };
        for r in 0..3 {
            for c in 0..3 {
                out[r][c] = (rows[r][c] * sign(r) * sign(c)) as f32;
            }
            out[r][3] = (rows[r][3] * sign(r)) as f32 * self.scale;
        }
        out[3][3] = 1.0;
        out
    }
}
