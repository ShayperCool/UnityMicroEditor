//! The avatar of a generic rig: the model's nodes as a skeleton (each by
//! the CRC-32 of its path, with its parent), their local transforms in
//! Unity's space as the skeleton's pose, and an empty human part with the
//! description's settings. Its size (`m_AvatarSize`, the constant's size in
//! memory) is taken over the release's tree when the object is written.
use crate::playerbake::{crc32, typetree::Val};

/// A node of the skeleton: its path below the root, its parent's position
/// and its local transform (translation, rotation `x y z w`, scale).
pub struct Bone {
    pub path: String,
    /// The node's own name, by which the skeleton's name IDs go.
    pub name: String,
    pub parent: i64,
    pub transform: ([f32; 3], [f32; 4], [f32; 3]),
}

/// The engine's default mass of each human bone, which a generic avatar
/// keeps in its human part.
const HUMAN_BONE_MASS: [f32; 25] = [
    0.14545456,
    0.12121213,
    0.12121213,
    0.048484854,
    0.048484854,
    0.009696971,
    0.009696971,
    0.030303033,
    0.14545456,
    0.14545456,
    0.012121214,
    0.048484854,
    0.006060607,
    0.006060607,
    0.024242427,
    0.024242427,
    0.01818182,
    0.01818182,
    0.006060607,
    0.006060607,
    0.0024242427,
    0.0024242427,
    0.0,
    0.0,
    0.0,
];

fn vector(v: [f32; 3]) -> Val {
    Val::map([
        ("x", Val::F(v[0])),
        ("y", Val::F(v[1])),
        ("z", Val::F(v[2])),
    ])
}

fn xform((t, q, s): ([f32; 3], [f32; 4], [f32; 3])) -> Val {
    Val::map([
        ("t", vector(t)),
        (
            "q",
            Val::map([
                ("x", Val::F(q[0])),
                ("y", Val::F(q[1])),
                ("z", Val::F(q[2])),
                ("w", Val::F(q[3])),
            ]),
        ),
        ("s", vector(s)),
    ])
}

const IDENTITY: ([f32; 3], [f32; 4], [f32; 3]) = ([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]);

fn skeleton(bones: &[Bone]) -> Val {
    Val::map([
        (
            "m_Node",
            Val::Seq(
                bones
                    .iter()
                    .map(|bone| {
                        Val::map([
                            ("m_ParentId", Val::I(bone.parent)),
                            ("m_AxesId", Val::I(-1)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "m_ID",
            Val::Seq(
                bones
                    .iter()
                    .map(|b| Val::U(crc32(&b.path) as u64))
                    .collect(),
            ),
        ),
        ("m_AxesArray", Val::Seq(Vec::new())),
    ])
}

fn empty_skeleton() -> Val {
    Val::map([
        ("m_Node", Val::Seq(Vec::new())),
        ("m_ID", Val::Seq(Vec::new())),
        ("m_AxesArray", Val::Seq(Vec::new())),
    ])
}

/// The description's settings: twists, stretches, feet spacing and scale.
#[derive(Debug, Clone)]
pub struct Description {
    pub arm_twist: f32,
    pub fore_arm_twist: f32,
    pub upper_leg_twist: f32,
    pub leg_twist: f32,
    pub arm_stretch: f32,
    pub leg_stretch: f32,
    pub feet_spacing: f32,
    pub global_scale: f32,
    pub has_translation_dof: bool,
    pub has_extra_root: bool,
    pub skeleton_has_parents: bool,
}

pub fn generic(name: &str, bones: &[Bone], description: &Description) -> Val {
    let pose = Val::map([(
        "m_X",
        Val::Seq(bones.iter().map(|b| xform(b.transform)).collect()),
    )]);
    let ids: Vec<Val> = bones
        .iter()
        .map(|b| Val::U(crc32(&b.name) as u64))
        .collect();
    let hand = || Val::map([("m_HandBoneIndex", Val::Seq(vec![Val::I(-1); 15]))]);
    let human = Val::map([
        ("m_RootX", xform(IDENTITY)),
        ("m_Skeleton", empty_skeleton()),
        ("m_SkeletonPose", Val::map([("m_X", Val::Seq(Vec::new()))])),
        ("m_LeftHand", hand()),
        ("m_RightHand", hand()),
        ("m_HumanBoneIndex", Val::Seq(vec![Val::I(-1); 25])),
        (
            "m_HumanBoneMass",
            Val::Seq(HUMAN_BONE_MASS.iter().map(|m| Val::F(*m)).collect()),
        ),
        ("m_Scale", Val::F(1.0)),
        ("m_ArmTwist", Val::F(description.arm_twist)),
        ("m_ForeArmTwist", Val::F(description.fore_arm_twist)),
        ("m_UpperLegTwist", Val::F(description.upper_leg_twist)),
        ("m_LegTwist", Val::F(description.leg_twist)),
        ("m_ArmStretch", Val::F(description.arm_stretch)),
        ("m_LegStretch", Val::F(description.leg_stretch)),
        ("m_FeetSpacing", Val::F(description.feet_spacing)),
        ("m_HasLeftHand", Val::Bool(false)),
        ("m_HasRightHand", Val::Bool(false)),
        ("m_HasTDoF", Val::Bool(false)),
    ]);
    let constant = Val::map([
        ("m_AvatarSkeleton", skeleton(bones)),
        ("m_AvatarSkeletonPose", pose.clone()),
        ("m_DefaultPose", pose),
        ("m_SkeletonNameIDArray", Val::Seq(ids)),
        ("m_Human", human),
        ("m_HumanSkeletonIndexArray", Val::Seq(Vec::new())),
        ("m_HumanSkeletonReverseIndexArray", Val::Seq(Vec::new())),
        ("m_RootMotionBoneIndex", Val::I(-1)),
        ("m_RootMotionBoneX", xform(IDENTITY)),
        ("m_RootMotionSkeleton", empty_skeleton()),
        (
            "m_RootMotionSkeletonPose",
            Val::map([("m_X", Val::Seq(Vec::new()))]),
        ),
        ("m_RootMotionSkeletonIndexArray", Val::Seq(Vec::new())),
    ]);
    let names = bones
        .iter()
        .map(|b| {
            Val::map([
                ("first", Val::U(crc32(&b.path) as u64)),
                ("second", Val::Str(b.path.clone())),
            ])
        })
        .collect();
    Val::map([
        ("m_Name", Val::Str(format!("{name}Avatar"))),
        // The size in memory, taken over the release's tree when written:
        // the Player reserves it before reading and takes more if needed.
        ("m_AvatarSize", Val::U(1)),
        ("m_Avatar", constant),
        ("m_TOS", Val::Seq(names)),
        (
            "m_HumanDescription",
            Val::map([
                ("m_Human", Val::Seq(Vec::new())),
                ("m_Skeleton", Val::Seq(Vec::new())),
                ("m_ArmTwist", Val::F(description.arm_twist)),
                ("m_ForeArmTwist", Val::F(description.fore_arm_twist)),
                ("m_UpperLegTwist", Val::F(description.upper_leg_twist)),
                ("m_LegTwist", Val::F(description.leg_twist)),
                ("m_ArmStretch", Val::F(description.arm_stretch)),
                ("m_LegStretch", Val::F(description.leg_stretch)),
                ("m_FeetSpacing", Val::F(description.feet_spacing)),
                ("m_GlobalScale", Val::F(description.global_scale)),
                ("m_RootMotionBoneName", Val::Str(String::new())),
                (
                    "m_HasTranslationDoF",
                    Val::Bool(description.has_translation_dof),
                ),
                ("m_HasExtraRoot", Val::Bool(description.has_extra_root)),
                (
                    "m_SkeletonHasParents",
                    Val::Bool(description.skeleton_has_parents),
                ),
            ]),
        ),
    ])
}
