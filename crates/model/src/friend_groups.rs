//! Device-local friend groups: a personal way to sort the friends list. Never sent to Discord.

/// Groups kept on this device.
pub const MAX_GROUPS: usize = 16;
/// Friends across all groups; one friend belongs to at most one group.
pub const MAX_MEMBERS: usize = 250;
/// Group names in bytes.
pub const MAX_NAME: usize = 48;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FriendGroup {
	pub name: String,
	/// User IDs, in the order they were added.
	pub members: Vec<u64>,
	pub collapsed: bool,
}

/// Bounds and uniqueness: names are non-empty and short, a friend sits in one group only.
pub fn friend_groups_valid(groups: &[FriendGroup]) -> bool {
	if groups.len() > MAX_GROUPS {
		return false;
	}
	let mut seen = std::collections::BTreeSet::new();
	let mut total = 0usize;
	for group in groups {
		let name = group.name.trim();
		if name.is_empty()
			|| group.name.len() > MAX_NAME
			|| group.name.chars().any(char::is_control)
		{
			return false;
		}
		total += group.members.len();
		if total > MAX_MEMBERS || group.members.iter().any(|id| *id == 0 || !seen.insert(*id)) {
			return false;
		}
	}
	true
}

/// Index of the group holding `user`, if any.
pub fn friend_group_of(groups: &[FriendGroup], user: u64) -> Option<usize> {
	groups
		.iter()
		.position(|group| group.members.contains(&user))
}

/// Moves `user` into `target` (or out of every group with `None`), keeping bounds.
/// Returns false when the move would exceed the member limit.
pub fn move_friend(groups: &mut [FriendGroup], user: u64, target: Option<usize>) -> bool {
	let current = friend_group_of(groups, user);
	if current == target || target.is_some_and(|index| index >= groups.len()) || user == 0 {
		return current == target;
	}
	let total: usize = groups.iter().map(|group| group.members.len()).sum();
	if current.is_none() && total >= MAX_MEMBERS {
		return false;
	}
	if let Some(index) = current {
		groups[index].members.retain(|id| *id != user);
	}
	if let Some(index) = target {
		groups[index].members.push(user);
	}
	true
}

#[cfg(test)]
mod tests {
	use super::*;

	fn group(name: &str, members: &[u64]) -> FriendGroup {
		FriendGroup {
			name: name.into(),
			members: members.to_vec(),
			collapsed: false,
		}
	}

	#[test]
	fn a_friend_belongs_to_one_group_and_moves_between_them() {
		let mut groups = vec![group("IUT", &[1, 2]), group("Minecraft", &[3])];
		assert!(friend_groups_valid(&groups));
		assert!(move_friend(&mut groups, 2, Some(1)));
		assert_eq!(groups[0].members, [1]);
		assert_eq!(groups[1].members, [3, 2]);
		assert!(move_friend(&mut groups, 3, None));
		assert_eq!(friend_group_of(&groups, 3), None);
		// Duplicates across groups are rejected when loading.
		assert!(!friend_groups_valid(&[group("A", &[5]), group("B", &[5])]));
		assert!(!friend_groups_valid(&[group("  ", &[])]));
	}

	#[test]
	fn member_limit_blocks_new_entries_but_not_moves() {
		let all: Vec<u64> = (1..=MAX_MEMBERS as u64).collect();
		let mut groups = vec![group("A", &all), group("B", &[])];
		assert!(friend_groups_valid(&groups));
		assert!(!move_friend(&mut groups, 9999, Some(0)));
		assert!(move_friend(&mut groups, 1, Some(1)));
	}
}
