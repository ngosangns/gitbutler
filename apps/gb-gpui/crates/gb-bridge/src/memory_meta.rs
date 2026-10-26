//! In-memory [`but_core::RefMetadata`]. Nothing here is written to disk.
//!
//! Copied in shape from GitButler's test support so the graph can see workspace
//! stacks without opening `BranchOrderMetadata`, whose "read-only" constructor
//! still takes a write transaction.

use std::any::Any;
use std::ops::{Deref, DerefMut};

use but_core::ref_metadata::{self, Branch, Workspace};
use gix::refs::FullName;

#[derive(Debug, Default)]
pub struct MemoryMeta {
    workspaces: Vec<(FullName, Workspace)>,
    branches: Vec<(FullName, Branch)>,
}

pub struct MemoryHandle<T> {
    is_default: bool,
    ref_name: FullName,
    value: T,
}

impl<T> AsRef<gix::refs::FullNameRef> for MemoryHandle<T> {
    fn as_ref(&self) -> &gix::refs::FullNameRef {
        self.ref_name.as_ref()
    }
}

impl<T> ref_metadata::ValueInfo for MemoryHandle<T> {
    fn is_default(&self) -> bool {
        self.is_default
    }
}

impl<T> Deref for MemoryHandle<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for MemoryHandle<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl but_core::RefMetadata for MemoryMeta {
    type Handle<T> = MemoryHandle<T>;

    fn iter(&self) -> impl Iterator<Item = anyhow::Result<(FullName, Box<dyn Any>)>> {
        self.workspaces
            .iter()
            .cloned()
            .map(|(name, value)| Ok((name, Box::new(value) as Box<dyn Any>)))
            .chain(
                self.branches
                    .iter()
                    .cloned()
                    .map(|(name, value)| Ok((name, Box::new(value) as Box<dyn Any>))),
            )
    }

    fn workspace(
        &self,
        ref_name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<Self::Handle<Workspace>> {
        Ok(self
            .workspaces
            .iter()
            .find_map(|(name, value)| {
                (name.as_ref() == ref_name).then(|| MemoryHandle {
                    is_default: false,
                    ref_name: ref_name.to_owned(),
                    value: value.clone(),
                })
            })
            .unwrap_or_else(|| MemoryHandle {
                is_default: true,
                ref_name: ref_name.to_owned(),
                value: Workspace::default(),
            }))
    }

    fn branch(&self, ref_name: &gix::refs::FullNameRef) -> anyhow::Result<Self::Handle<Branch>> {
        Ok(self
            .branches
            .iter()
            .find_map(|(name, value)| {
                (name.as_ref() == ref_name).then(|| MemoryHandle {
                    is_default: false,
                    ref_name: ref_name.to_owned(),
                    value: value.clone(),
                })
            })
            .unwrap_or_else(|| MemoryHandle {
                is_default: true,
                ref_name: ref_name.to_owned(),
                value: Branch::default(),
            }))
    }

    fn set_workspace(&mut self, value: &Self::Handle<Workspace>) -> anyhow::Result<()> {
        match self
            .workspaces
            .iter_mut()
            .find(|entry| entry.0 == value.ref_name)
        {
            None => self
                .workspaces
                .push((value.ref_name.clone(), value.value.clone())),
            Some(existing) => existing.1 = value.value.clone(),
        }
        Ok(())
    }

    fn set_branch(&mut self, value: &Self::Handle<Branch>) -> anyhow::Result<()> {
        match self
            .branches
            .iter_mut()
            .find(|entry| entry.0 == value.ref_name)
        {
            None => self
                .branches
                .push((value.ref_name.clone(), value.value.clone())),
            Some(existing) => existing.1 = value.value.clone(),
        }
        Ok(())
    }

    fn remove(&mut self, ref_name: &gix::refs::FullNameRef) -> anyhow::Result<bool> {
        let branch_pos = self
            .branches
            .iter()
            .position(|(name, _)| name.as_ref() == ref_name);
        let workspace_pos = self
            .workspaces
            .iter()
            .position(|(name, _)| name.as_ref() == ref_name);
        let removed = match (branch_pos, workspace_pos) {
            (Some(branch), Some(workspace)) => {
                self.branches.remove(branch);
                self.workspaces.remove(workspace);
                true
            }
            (None, Some(workspace)) => {
                self.workspaces.remove(workspace);
                true
            }
            (Some(branch), None) => {
                self.branches.remove(branch);
                true
            }
            (None, None) => false,
        };
        Ok(removed)
    }

    fn rename(
        &mut self,
        old_ref_name: &gix::refs::FullNameRef,
        new_ref_name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<()> {
        if old_ref_name == new_ref_name {
            return Ok(());
        }
        let new_exists = self
            .branches
            .iter()
            .any(|(name, _)| name.as_ref() == new_ref_name)
            || self
                .workspaces
                .iter()
                .any(|(name, _)| name.as_ref() == new_ref_name);
        if new_exists {
            anyhow::bail!(
                "Cannot rename to '{}': metadata for that ref already exists",
                new_ref_name.shorten()
            );
        }
        for (name, _) in self.branches.iter_mut() {
            if name.as_ref() == old_ref_name {
                *name = new_ref_name.to_owned();
            }
        }
        for (name, _) in self.workspaces.iter_mut() {
            if name.as_ref() == old_ref_name {
                *name = new_ref_name.to_owned();
            }
        }
        Ok(())
    }
}
