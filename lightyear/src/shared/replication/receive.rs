//! General struct handling replication
use std::collections::BTreeMap;

use bevy::ecs::entity::EntityHash;
use bevy::prelude::{DespawnRecursiveExt, Entity, World};
use bevy::utils::HashSet;
use tracing::{debug, error, trace, warn};
#[cfg(feature = "trace")]
use tracing::{instrument, Level};

use crate::packet::message::MessageId;
use crate::prelude::client::Confirmed;
use crate::prelude::{ClientId, Tick};
use crate::protocol::component::ComponentRegistry;
use crate::serialize::reader::Reader;
use crate::shared::events::connection::ConnectionEvents;
use crate::shared::replication::components::{Replicated, ReplicationGroupId};
#[cfg(test)]
use crate::utils::captures::Captures;

use super::entity_map::RemoteEntityMap;
use super::{EntityActionsMessage, EntityUpdatesMessage, SpawnAction};

type EntityHashMap<K, V> = hashbrown::HashMap<K, V, EntityHash>;

type EntityHashSet<K> = hashbrown::HashSet<K, EntityHash>;

pub(crate) struct ReplicationReceiver {
    /// Map between local and remote entities. (used mostly on client because it's when we receive entity updates)
    pub remote_entity_map: RemoteEntityMap,

    /// Map from remote entity to the replication group-id
    pub remote_entity_to_group: EntityHashMap<Entity, ReplicationGroupId>,

    // BOTH
    /// Buffer to so that we have an ordered receiver per group
    pub group_channels: EntityHashMap<ReplicationGroupId, GroupChannel>,
}

impl ReplicationReceiver {
    pub(crate) fn new() -> Self {
        Self {
            // RECEIVE
            remote_entity_map: RemoteEntityMap::default(),
            remote_entity_to_group: Default::default(),
            // BOTH
            group_channels: Default::default(),
        }
    }

    /// Buffer a received [`EntityActionsMessage`].
    ///
    /// The remote_tick is the tick at which the message was buffered and sent by the remote client.
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn recv_actions(&mut self, actions: EntityActionsMessage, remote_tick: Tick) {
        trace!(
            ?actions,
            ?remote_tick,
            "Received ReplicationActions message"
        );
        let channel = self.group_channels.entry(actions.group_id).or_default();

        // if the message is too old, ignore it
        if actions.sequence_id < channel.actions_pending_recv_message_id {
            trace!(message_id= ?actions.sequence_id, pending_message_id = ?channel.actions_pending_recv_message_id, "message is too old, ignored");
            return;
        }
        // update the list of entities in the group
        actions
            .actions
            .iter()
            .map(|(entity, _)| entity)
            .for_each(|entity| {
                channel.remote_entities.insert(*entity);
            });

        // add the message to the buffer
        // TODO: I guess this handles potential duplicates?
        channel
            .actions_recv_message_buffer
            .insert(actions.sequence_id, (remote_tick, actions));
        trace!(?channel, "group channel after buffering");
    }

    /// Buffer a received [`EntityUpdatesMessage`].
    ///
    /// The remote_tick is the tick at which the message was buffered and sent by the remote client.
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    pub(crate) fn recv_updates(&mut self, updates: EntityUpdatesMessage, remote_tick: Tick) {
        trace!(?updates, ?remote_tick, "Received replication message");
        let channel = self.group_channels.entry(updates.group_id).or_default();

        // NOTE: this is valid even after tick wrapping because we keep clamping the latest_tick values for each channel
        // if we have already applied a more recent update for this group, no need to keep this one (or should we keep it for history?)
        if channel.latest_tick.is_some_and(|t| remote_tick <= t) {
            return;
        }

        // TODO: what we want is
        //  - if the update is for a tick in the past compared to our local state, we can safely ignore immediately
        //  - make sure that the local state has a `latest_tick` that is bigger than the update's remote tick (i.e.
        //  we only apply remote ticks if we have reached the last_action_tick for that update)
        //  - if we have two updates that satisfy those conditions, we don't need to buffer both!
        //   We can just keep the one with the biggest last_action_tick? since eventually that's the only one we're going to apply.
        //   Possible exceptions:
        //   - we want to keep all the intermediary information to put it in a history for interpolation (so that instead of interpolating
        //     only between the updates we apply that have the highest tick, we interpolate between all updates received. The interpolation
        //     tick could be much further in the past. Or maybe check the interpolation tick?)
        //   - we could be delaying some intermediary updates because the update with higher tick also has a higher last_action_tick,
        //     and we might have some intermediary updates that we could be applying.
        //     For example `latest_tick` is 5, we receive an update from tick 20 with last_action_tick = 15, and we receive an update
        //     from tick 10 with last_action tick = 7. Even If we receive the action_tick for tick 7, we wouldn't be able to apply it right away
        //     because we're waiting for the action_tick for tick 15. So we should keep both updates, and apply them as soon as possible (as soon
        //     as the smallest last_action_tick is reached)
        //     However in practice this seems expensive to do, and a rare case. For now, let's just only keep the update with the highest tick?
        //     TODO: check that this is correct even with delta_compression.

        // TODO: could we use a FreeList here? (SequenceBuffer?) Updates are only buffered until we reach their last_action_tick
        //  which should be fairly quick, never more than 1-2 sec. (so a buffer of size 64 or 128 seems good). It might need more memory though?
        //  Benchmark.
        channel.buffered_updates.insert(updates, remote_tick);

        // TODO: include somewhere in the update message the m.last_ack_tick since when we compute changes?
        //  (if we want to do diff compression?)
        trace!(?channel, "group channel after buffering");
    }

    /// Return all the [`EntityActionsMessage`] from our internal buffer that are ready to be read.
    /// For each [`ReplicationGroup`], we return the actions in order.
    ///
    /// (i.e. if we have sent an action for tick 3 and tick 7, we wait until we receive the one for tick 3 first)
    #[cfg(test)]
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    fn read_actions(
        &mut self,
        current_tick: Tick,
    ) -> impl Iterator<Item = (Tick, EntityActionsMessage)> + Captures<&()> {
        trace!(?current_tick, ?self.group_channels, "reading replication messages");
        self.group_channels
            .iter_mut()
            .flat_map(move |(group_id, channel)| channel.read_actions(current_tick))
    }

    #[cfg(test)]
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    fn read_updates(&mut self) -> impl Iterator<Item = Update> + Captures<&()> {
        trace!(?self.group_channels, "reading replication messages");
        self.group_channels
            .iter_mut()
            .flat_map(|(group_id, channel)| channel.read_updates())
    }

    /// Gets the tick at which the provided confirmed entity currently is
    /// (i.e. the latest server tick at which we received an update for that entity)
    pub(crate) fn get_confirmed_tick(&self, confirmed_entity: Entity) -> Option<Tick> {
        self.channel_by_local(confirmed_entity)
            .and_then(|channel| channel.latest_tick)
    }

    /// Get the replication group id associated with a given local entity
    pub(crate) fn get_replication_group_id(
        &self,
        confirmed_entity: Entity,
    ) -> Option<ReplicationGroupId> {
        self.remote_entity_map
            .get_remote(confirmed_entity)
            .and_then(|remote_entity| self.remote_entity_to_group.get(remote_entity))
            .copied()
    }

    // USED BY RECEIVE SIDE (SEND SIZE CAN GET THE GROUP_ID EASILY)
    /// Get the group channel associated with a given entity
    fn channel_by_local(&self, local_entity: Entity) -> Option<&GroupChannel> {
        self.remote_entity_map
            .get_remote(local_entity)
            .and_then(|remote_entity| self.channel_by_remote(*remote_entity))
    }

    // USED BY RECEIVE SIDE (SEND SIZE CAN GET THE GROUP_ID EASILY)
    /// Get the group channel associated with a given entity
    fn channel_by_remote(&self, remote_entity: Entity) -> Option<&GroupChannel> {
        self.remote_entity_to_group
            .get(&remote_entity)
            .and_then(|group_id| self.group_channels.get(group_id))
    }

    /// Do some internal bookkeeping:
    /// - handle tick wrapping
    pub(crate) fn cleanup(&mut self, tick: Tick) {
        // if it's been enough time since we last had any update for the group, we update the latest_tick for the group
        for group_channel in self.group_channels.values_mut() {
            debug!("Checking group channel: {:?}", group_channel);
            if let Some(latest_tick) = group_channel.latest_tick {
                // delta = u16::MAX / 4
                if tick - latest_tick > (i16::MAX / 2) {
                    debug!(
                    ?tick,
                    ?latest_tick,
                    ?group_channel,
                    "Setting the latest_tick tick to tick because there hasn't been any new updates in a while");
                    group_channel.latest_tick = Some(tick);
                }
            }
        }
    }
}

/// We want:
/// - entity actions to be done reliably
/// - entity updates (component updates) to be done unreliably
///
/// - all component inserts/removes/updates for an entity to be grouped together in a single message
impl ReplicationReceiver {
    #[cfg(test)]
    pub(crate) fn apply_actions_message(
        &mut self,
        world: &mut World,
        remote: Option<ClientId>,
        component_registry: &ComponentRegistry,
        remote_tick: Tick,
        message: EntityActionsMessage,
        events: &mut ConnectionEvents,
    ) {
        let group_id = message.group_id;
        debug!(?remote_tick, ?message, "Received replication actions");
        // NOTE: order matters here, because some components can depend on other entities.
        // These components could even form a cycle, for example A.HasWeapon(B) and B.HasHolder(A)
        // Our solution is to first handle spawn for all entities separately.
        for (remote_entity, actions) in message.actions.iter() {
            debug!(?remote_entity, "Received entity actions");
            // spawn
            match actions.spawn {
                SpawnAction::Spawn => {
                    self.remote_entity_to_group.insert(*remote_entity, group_id);
                    if let Some(local_entity) = self.remote_entity_map.get_local(*remote_entity) {
                        if world.get_entity(*local_entity).is_some() {
                            warn!("Received spawn for an entity that already exists");
                            continue;
                        }
                        warn!("Received spawn for an entity that is already in our entity mapping! Not spawning");
                        continue;
                    }
                    // TODO: optimization: spawn the bundle of insert components

                    // TODO: spawning all entities with Confirmed:
                    //  - is inefficient because we don't need the receive tick in most cases (only for prediction/interpolation)
                    //  - we can't use Without<Confirmed> queries to display all interpolated/predicted entities, because
                    //    the entities we receive from other clients all have Confirmed added.
                    //    Doing Or<(With<Interpolated>, With<Predicted>)> is not ideal; what if we want to see a replicated entity that doesn't have
                    //    interpolation/prediction? Maybe we should introduce new components ReplicatedFrom<Server> and ReplicatedFrom<Client>.
                    // // we spawn every replicated entity with the `Confirmed` component
                    // let local_entity = world.spawn(Confirmed {
                    //     predicted: None,
                    //     interpolated: None,
                    //     tick,
                    // });
                    let local_entity = world.spawn(Replicated { from: remote });
                    self.remote_entity_map
                        .insert(*remote_entity, local_entity.id());
                    trace!("Updated remote entity map: {:?}", self.remote_entity_map);

                    debug!(?remote_entity, "Received entity spawn");
                    events.push_spawn(local_entity.id());
                }
                SpawnAction::Reuse(local_entity) => {
                    let Some(mut entity_mut) = world.get_entity_mut(local_entity) else {
                        // TODO: ignore the entity in the next steps because it does not exist!
                        error!("Received ReuseEntity({local_entity:?}) but the entity does not exist in the world");
                        continue;
                    };
                    entity_mut.insert(Replicated { from: remote });
                    // update the entity mapping
                    self.remote_entity_map.insert(*remote_entity, local_entity);
                }
                _ => {}
            }
        }

        for (entity, actions) in message.actions.into_iter() {
            debug!(remote_entity = ?entity, "Received entity actions");

            // despawn
            if actions.spawn == SpawnAction::Despawn {
                debug!(remote_entity = ?entity, "Received entity despawn");
                if let Some(local_entity) = self.remote_entity_map.remove_by_remote(entity) {
                    if let Some(group) = self.group_channels.get_mut(&group_id) {
                        group.remote_entities.remove(&entity);
                    }
                    // TODO: we despawn all children as well right now, but that might not be what we want?
                    if let Some(entity_mut) = world.get_entity_mut(local_entity) {
                        entity_mut.despawn_recursive();
                    }
                    events.push_despawn(local_entity);
                    self.remote_entity_to_group.remove(&entity);
                } else {
                    error!("Received despawn for an entity that does not exist")
                }
                continue;
            }

            // safety: we know by this point that the entity exists
            let Some(mut local_entity_mut) = self.remote_entity_map.get_by_remote(world, entity)
            else {
                error!("cannot find entity");
                continue;
            };

            // NOTE: 2 options
            //  - send the raw data to a separate typed system
            //  -  or just insert it here via function pointers

            // inserts
            // TODO: remove updates that are duplicate for the same component
            debug!(remote_entity = ?entity, "Received InsertComponent");
            for component in actions.insert {
                // TODO: we allocate a new vector for each component but we should
                //  be able to re-use the same reader
                let mut reader = Reader::from(component);
                let _ = component_registry
                    .raw_write(
                        &mut reader,
                        &mut local_entity_mut,
                        remote_tick,
                        &mut self.remote_entity_map.remote_to_local,
                        events,
                    )
                    .inspect_err(|e| {
                        error!("could not write the component to the entity: {:?}", e)
                    });

                // TODO: special-case for pre-spawned entities: we receive them from a client, but then we
                //  we should immediately take ownership of it, so we won't receive a despawn for it
                //  thus, we should remove it from the entity map right after receiving it!
                //  Actually, we should figure out a way to cleanup every received entity where the sender
                //  stopped replicating or didn't replicate the Despawn, as this could just cause memory to accumulate

                // TODO: maybe if is-server, attach the client-id to the ShouldBePredicted entity
                //  to know for which client we should do the pre-prediction
            }

            // removals
            trace!(remote_entity = ?entity, ?actions.remove, "Received RemoveComponent");
            for kind in actions.remove {
                events.push_remove_component(local_entity_mut.id(), kind, Tick(0));
                component_registry.raw_remove(kind, &mut local_entity_mut);
            }

            // updates
            debug!(remote_entity = ?entity, "Received UpdateComponent");
            for component in actions.updates {
                // TODO: re-use buffers via pool?
                let mut reader = Reader::from(component);
                let _ = component_registry
                    .raw_write(
                        &mut reader,
                        &mut local_entity_mut,
                        remote_tick,
                        &mut self.remote_entity_map.remote_to_local,
                        events,
                    )
                    .inspect_err(|e| {
                        error!("could not write the component to the entity: {:?}", e)
                    });
            }
        }
        self.update_confirmed_tick(world, group_id, remote_tick);
    }

    #[cfg(test)]
    pub(crate) fn apply_updates_message(
        &mut self,
        world: &mut World,
        remote: Option<ClientId>,
        component_registry: &ComponentRegistry,
        remote_tick: Tick,
        is_history: bool,
        message: EntityUpdatesMessage,
        events: &mut ConnectionEvents,
    ) {
        let group_id = message.group_id;
        debug!(?remote_tick, ?message, "Received replication updates");
        // TODO: store this in ConfirmedHistory?
        if is_history {
            return;
        }
        for (entity, components) in message.updates.into_iter() {
            debug!(?components, remote_entity = ?entity, "Received UpdateComponent");
            // update the entity only if it exists
            if let Some(mut local_entity_mut) = self.remote_entity_map.get_by_remote(world, entity)
            {
                for component in components {
                    let mut reader = Reader::from(component);
                    let _ = component_registry
                        .raw_write(
                            &mut reader,
                            &mut local_entity_mut,
                            remote_tick,
                            &mut self.remote_entity_map.remote_to_local,
                            events,
                        )
                        .inspect_err(|e| {
                            error!("could not write the component to the entity: {:?}", e)
                        });
                }
            } else {
                // we can get a few buffered updates after the entity has been despawned
                // those are the updates that we received before the despawn action message, but with a tick
                // later than the despawn action message
                debug!("update for entity that doesn't exist?");
            }
        }
        self.update_confirmed_tick(world, group_id, remote_tick);
    }

    /// Update the Confirmed tick for all entities in the replication group
    /// so that Predicted/Interpolated entities can be notified
    ///
    /// We update it for all entities in the group (even if we received only an update that contains
    /// updates for E1, it also means that E2 is updated to the same tick, since they are part of the
    /// same group)
    pub(crate) fn update_confirmed_tick(
        &mut self,
        world: &mut World,
        group_id: ReplicationGroupId,
        remote_tick: Tick,
    ) {
        // TODO: maybe get the confirmed tick from the apply_world message directly?
        // // let confirmed_tick = self.group_channels.get(&group_id).unwrap().latest_tick;
        // if let Some(group_channel) = self.group_channels
        //     .get(&group_id) {
        //     grou.remote_entities
        //
        // }

        if let Some(g) = self.group_channels.get(&group_id) {
            g.remote_entities.iter().for_each(|remote_entity| {
                if let Some(mut local_entity_mut) =
                    self.remote_entity_map.get_by_remote(world, *remote_entity)
                {
                    trace!(?remote_tick, "updating confirmed tick for entity");
                    if let Some(mut confirmed) = local_entity_mut.get_mut::<Confirmed>() {
                        confirmed.tick = remote_tick;
                    }
                }
            });
        }
    }

    // TODO: how can I emit metrics here that contain the channel kind?
    //  use a OnceCell that gets set with the channel name mapping when the protocol is finalized?
    //  the other option is to have wrappers in Connection, but that's pretty ugly

    /// Read from the buffer the EntityActionsMessage and EntityUpdatesMessage that are ready,
    /// and apply them to the World
    #[cfg_attr(feature = "trace", instrument(level = Level::INFO, skip_all))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_world(
        &mut self,
        // TODO: should we use Commands to avoid the need to block the world?
        world: &mut World,
        remote: Option<ClientId>,
        component_registry: &ComponentRegistry,
        current_tick: Tick,
        events: &mut ConnectionEvents,
    ) {
        // apply actions first

        // TODO: this would be how we do it, but the borrow-checked prevents us...
        //  either create a ViewLens?

        // self.read_actions(current_tick)
        //     .for_each(|(remote_tick, actions)| {
        //         self.apply_actions_message(
        //             world,
        //             remote,
        //             component_registry,
        //             remote_tick,
        //             actions,
        //             events,
        //         )
        //     });
        // // then updates
        // self.read_updates().for_each(|update| {
        //     self.apply_updates_message(
        //         world,
        //         remote,
        //         component_registry,
        //         update.remote_tick,
        //         update.is_history,
        //         update.message,
        //         events,
        //     )
        // });

        trace!(?current_tick, ?self.group_channels, "applying replication actions messages");
        self.group_channels
            .iter_mut()
            .for_each(|(group_id, channel)| {
                let Some((remote_tick, _)) = channel
                    .actions_recv_message_buffer
                    .get(&channel.actions_pending_recv_message_id)
                else {
                    return;
                };
                // if the message is from the future, keep it there
                if *remote_tick > current_tick {
                    debug!(
                        "message tick {:?} is from the future compared to our current tick {:?}",
                        remote_tick, current_tick
                    );
                    return;
                }

                // We have received the message we are waiting for
                let (remote_tick, message) = channel
                    .actions_recv_message_buffer
                    .remove(&channel.actions_pending_recv_message_id)
                    .unwrap();

                channel.actions_pending_recv_message_id += 1;
                // Update the latest server tick that we have processed
                channel.latest_tick = Some(remote_tick);

                channel.apply_actions_message(
                    world,
                    remote,
                    component_registry,
                    remote_tick,
                    message,
                    events,
                    &mut self.remote_entity_map,
                    &mut self.remote_entity_to_group,
                );
            });

        trace!(?self.group_channels, "applying replication updates messages");
        self.group_channels
            .iter_mut()
            .for_each(|(group_id, channel)| {
                // the buffered_channel is sorted in descending order,
                // [most_recent_tick, ...,  max_readable_tick (based on last_action_tick), ..., oldest_tick]
                // What we want is to return (not necessarily in order) [max_readable_tick, ..., oldest_tick]
                // along with a flag that lets us know if we are the max_readable_tick or not.
                // (max_readable_tick is the only one we want to actually apply to the world, because the other
                //  older updates are redundant. The older ticks are included so that we can have a comprehensive
                //  confirmed history, for example to have a better interpolation)
                let Some(max_applicable_idx) = channel
                    .buffered_updates
                    .max_index_to_apply(channel.latest_tick)
                else {
                    return;
                };

                // pop the oldest until we reach the max applicable index
                while channel.buffered_updates.len() > max_applicable_idx {
                    let (remote_tick, message) = channel.buffered_updates.pop_oldest().unwrap();
                    let is_history = channel.buffered_updates.len() != max_applicable_idx;
                    channel.apply_updates_message(
                        world,
                        remote,
                        component_registry,
                        remote_tick,
                        is_history,
                        message,
                        events,
                        &mut self.remote_entity_map,
                    );
                }
            })
    }
}

/// Channel to keep track of receiving/sending replication messages for a given Group
#[derive(Debug)]
pub struct GroupChannel {
    // entities
    // set of remote entities that are part of the same Replication Group
    remote_entities: HashSet<Entity>,
    // actions
    pub(crate) actions_pending_recv_message_id: MessageId,
    pub(crate) actions_recv_message_buffer: BTreeMap<MessageId, (Tick, EntityActionsMessage)>,
    // updates
    pub(crate) buffered_updates: UpdatesBuffer,
    /// remote tick of the latest update/action that we applied to the local group
    pub latest_tick: Option<Tick>,
}

impl Default for GroupChannel {
    fn default() -> Self {
        Self {
            remote_entities: HashSet::default(),
            actions_pending_recv_message_id: MessageId(0),
            actions_recv_message_buffer: BTreeMap::new(),
            buffered_updates: UpdatesBuffer::default(),
            latest_tick: None,
        }
    }
}

/// Iterator that returns all the available EntityActions for the current [`GroupChannel`]
///
/// Reads a message from the internal buffer to get its content
/// Since we are receiving messages in order, we don't return from the buffer
/// until we have received the message we are waiting for (the next expected MessageId)
/// This assumes that the sender sends all message ids sequentially.
///
/// If had received updates that were waiting on a given action, we also return them
struct ActionsIterator<'a> {
    channel: &'a mut GroupChannel,
    current_tick: Tick,
}

impl<'a> Iterator for ActionsIterator<'a> {
    /// The message along with the tick at which the remote message was sent
    type Item = (Tick, EntityActionsMessage);

    fn next(&mut self) -> Option<Self::Item> {
        // TODO: maybe only get the message if our local client tick is >= to it? (so that we don't apply an update from the future)
        let message = self
            .channel
            .actions_recv_message_buffer
            .get(&self.channel.actions_pending_recv_message_id)?;
        // if the message is from the future, keep it there
        if message.0 > self.current_tick {
            debug!(
                "message tick {:?} is from the future compared to our current tick {:?}",
                message.0, self.current_tick
            );
            return None;
        }

        // We have received the message we are waiting for
        let message = self
            .channel
            .actions_recv_message_buffer
            .remove(&self.channel.actions_pending_recv_message_id)
            .unwrap();

        self.channel.actions_pending_recv_message_id += 1;
        // Update the latest server tick that we have processed
        self.channel.latest_tick = Some(message.0);
        Some(message)
    }
}

// TODO: try a sequence buffer?
/// Stores the [`EntityUpdatesMessage`] for a given [`ReplicationGroup`](crate::prelude::ReplicationGroup), sorted
/// in descending remote tick order (the most recent tick first, the oldest tick last)
///
/// The first element is the remote tick, the second is the message
#[derive(Debug)]
pub struct UpdatesBuffer(Vec<(Tick, EntityUpdatesMessage)>);

/// Update that is given to `apply_world`
#[derive(Debug, PartialEq)]
struct Update {
    remote_tick: Tick,
    message: EntityUpdatesMessage,
    /// If true, we don't want to apply the update to the world, because we are going
    /// to apply a more recent one
    is_history: bool,
}
impl Default for UpdatesBuffer {
    fn default() -> Self {
        Self(Vec::with_capacity(1))
    }
}
impl UpdatesBuffer {
    fn clear(&mut self) {
        self.0.clear();
    }

    /// Insert a new message in the right position to make sure that the buffer
    /// is still sorted in descending order
    fn insert(&mut self, message: EntityUpdatesMessage, remote_tick: Tick) {
        let index = self.0.partition_point(|(tick, _)| remote_tick < *tick);
        self.0.insert(index, (remote_tick, message));
    }

    /// Number of messages in the buffer
    fn len(&self) -> usize {
        self.0.len()
    }

    /// Get the index of the most recent element in the buffer which has a last_action_tick <= latest_tick,
    /// i.e. which can be applied that has the highest tick that is less than or equal to the latest_tick
    ///
    /// or None if there are None
    fn max_index_to_apply(&self, latest_tick: Option<Tick>) -> Option<usize> {
        // if we haven't applied any latest_tick, we can't apply any updates
        let latest_tick = latest_tick?;

        // we can use partition point because we know that all the non-ready elements will be on the left
        // and the ready elements will be on the right
        let idx = self.0.partition_point(|(_, message)| {
            let Some(last_action_tick) = message.last_action_tick else {
                return false;
            };
            last_action_tick > latest_tick
        });
        if idx == self.len() {
            None
        } else {
            Some(idx)
        }
    }
    /// Pop the oldest tick from the buffer
    fn pop_oldest(&mut self) -> Option<(Tick, EntityUpdatesMessage)> {
        self.0.pop()
    }
}

/// Iterator that returns all the available [`EntityUpdatesMessage`] for the current [`GroupChannel`]
///
/// We read from the [`UpdatesBuffer`] in ascending remote tick order:
/// - if we have not reached the last_action_tick for a given update, we stop there
/// - else, we return all the updates whose last_action_tick is reached, and
struct UpdatesIterator<'a> {
    channel: &'a mut GroupChannel,
    /// We iterate until we reach this idx in the buffer
    max_applicable_idx: Option<usize>,
}

impl<'a> Iterator for UpdatesIterator<'a> {
    /// The message along with the tick at which the remote message was sent
    type Item = Update;

    fn next(&mut self) -> Option<Self::Item> {
        // TODO: NEED TO REIMPLEMENT THIS TODO!
        // TODO: maybe only get the message if our local client tick is >= to it? (so that we don't apply an update from the future)

        // TODO: ideally we do this update only once, when instantiating the iterator?
        // if we cannot apply any updates, return None
        let max_applicable_idx = self.max_applicable_idx?;

        // we have returned all the items that were ready, stop now
        if self.channel.buffered_updates.len() == max_applicable_idx {
            return None;
        }

        // pop the oldest until we reach the max applicable index
        let (remote_tick, message) = self.channel.buffered_updates.pop_oldest().unwrap();
        Some(Update {
            remote_tick,
            message,
            is_history: self.channel.buffered_updates.len() != max_applicable_idx,
        })
    }
}

impl GroupChannel {
    /// Builds an iterator that returns all the available EntityActions for the current [`GroupChannel`]
    fn read_actions(&mut self, current_tick: Tick) -> ActionsIterator {
        ActionsIterator {
            channel: self,
            current_tick,
        }
    }

    /// Builds an iterator that returns all the available EntityUpdates for the current [`GroupChannel`]
    /// Needs to run after read_actions for correctness (because we need to update the `latest_tick` of
    /// the group before we can apply the updates)
    fn read_updates(&mut self) -> UpdatesIterator {
        // the buffered_channel is sorted in descending order,
        // [most_recent_tick, ...,  max_readable_tick (based on last_action_tick), ..., oldest_tick]
        // What we want is to return (not necessarily in order) [max_readable_tick, ..., oldest_tick]
        // along with a flag that lets us know if we are the max_readable_tick or not.
        // (max_readable_tick is the only one we want to actually apply to the world, because the other
        //  older updates are redundant. The older ticks are included so that we can have a comprehensive
        //  confirmed history, for example to have a better interpolation)
        let max_applicable_idx = self.buffered_updates.max_index_to_apply(self.latest_tick);

        UpdatesIterator {
            channel: self,
            max_applicable_idx,
        }
    }

    /// Apply actions for channel
    pub(crate) fn apply_actions_message(
        &mut self,
        world: &mut World,
        remote: Option<ClientId>,
        component_registry: &ComponentRegistry,
        remote_tick: Tick,
        message: EntityActionsMessage,
        events: &mut ConnectionEvents,
        remote_entity_map: &mut RemoteEntityMap,
        remote_entity_to_group: &mut EntityHashMap<Entity, ReplicationGroupId>,
    ) {
        let group_id = message.group_id;
        debug!(?remote_tick, ?message, "Received replication actions");
        // NOTE: order matters here, because some components can depend on other entities.
        // These components could even form a cycle, for example A.HasWeapon(B) and B.HasHolder(A)
        // Our solution is to first handle spawn for all entities separately.
        for (remote_entity, actions) in message.actions.iter() {
            debug!(?remote_entity, "Received entity actions");
            // spawn
            match actions.spawn {
                SpawnAction::Spawn => {
                    remote_entity_to_group.insert(*remote_entity, group_id);
                    if let Some(local_entity) = remote_entity_map.get_local(*remote_entity) {
                        if world.get_entity(*local_entity).is_some() {
                            warn!("Received spawn for an entity that already exists");
                            continue;
                        }
                        warn!("Received spawn for an entity that is already in our entity mapping! Not spawning");
                        continue;
                    }
                    // TODO: optimization: spawn the bundle of insert components

                    // TODO: spawning all entities with Confirmed:
                    //  - is inefficient because we don't need the receive tick in most cases (only for prediction/interpolation)
                    //  - we can't use Without<Confirmed> queries to display all interpolated/predicted entities, because
                    //    the entities we receive from other clients all have Confirmed added.
                    //    Doing Or<(With<Interpolated>, With<Predicted>)> is not ideal; what if we want to see a replicated entity that doesn't have
                    //    interpolation/prediction? Maybe we should introduce new components ReplicatedFrom<Server> and ReplicatedFrom<Client>.
                    // // we spawn every replicated entity with the `Confirmed` component
                    // let local_entity = world.spawn(Confirmed {
                    //     predicted: None,
                    //     interpolated: None,
                    //     tick,
                    // });
                    let local_entity = world.spawn(Replicated { from: remote });
                    remote_entity_map.insert(*remote_entity, local_entity.id());
                    trace!("Updated remote entity map: {:?}", remote_entity_map);

                    debug!(?remote_entity, "Received entity spawn");
                    events.push_spawn(local_entity.id());
                }
                SpawnAction::Reuse(local_entity) => {
                    let Some(mut entity_mut) = world.get_entity_mut(local_entity) else {
                        // TODO: ignore the entity in the next steps because it does not exist!
                        error!("Received ReuseEntity({local_entity:?}) but the entity does not exist in the world");
                        continue;
                    };
                    entity_mut.insert(Replicated { from: remote });
                    // update the entity mapping
                    remote_entity_map.insert(*remote_entity, local_entity);
                }
                _ => {}
            }
        }

        for (entity, actions) in message.actions.into_iter() {
            debug!(remote_entity = ?entity, "Received entity actions");

            // despawn
            if actions.spawn == SpawnAction::Despawn {
                debug!(remote_entity = ?entity, "Received entity despawn");
                if let Some(local_entity) = remote_entity_map.remove_by_remote(entity) {
                    self.remote_entities.remove(&entity);
                    // TODO: we despawn all children as well right now, but that might not be what we want?
                    if let Some(entity_mut) = world.get_entity_mut(local_entity) {
                        entity_mut.despawn_recursive();
                    }
                    events.push_despawn(local_entity);
                    remote_entity_to_group.remove(&entity);
                } else {
                    error!("Received despawn for an entity that does not exist")
                }
                continue;
            }

            // safety: we know by this point that the entity exists
            let Some(mut local_entity_mut) = remote_entity_map.get_by_remote(world, entity) else {
                error!(?entity, "cannot find entity");
                continue;
            };

            // NOTE: 2 options
            //  - send the raw data to a separate typed system
            //  -  or just insert it here via function pointers

            // inserts
            // TODO: remove updates that are duplicate for the same component
            debug!(remote_entity = ?entity, "Received InsertComponent");
            for component in actions.insert {
                // TODO: reuse a single reader that reads through the entire message
                let mut reader = Reader::from(component);
                let _ = component_registry
                    .raw_write(
                        &mut reader,
                        &mut local_entity_mut,
                        remote_tick,
                        &mut remote_entity_map.remote_to_local,
                        events,
                    )
                    .inspect_err(|e| {
                        error!("could not write the component to the entity: {:?}", e)
                    });

                // TODO: special-case for pre-spawned entities: we receive them from a client, but then we
                //  we should immediately take ownership of it, so we won't receive a despawn for it
                //  thus, we should remove it from the entity map right after receiving it!
                //  Actually, we should figure out a way to cleanup every received entity where the sender
                //  stopped replicating or didn't replicate the Despawn, as this could just cause memory to accumulate

                // TODO: maybe if is-server, attach the client-id to the ShouldBePredicted entity
                //  to know for which client we should do the pre-prediction
            }

            // removals
            trace!(remote_entity = ?entity, ?actions.remove, "Received RemoveComponent");
            for kind in actions.remove {
                events.push_remove_component(local_entity_mut.id(), kind, Tick(0));
                component_registry.raw_remove(kind, &mut local_entity_mut);
            }

            // updates
            debug!(remote_entity = ?entity, "Received UpdateComponent");
            for component in actions.updates {
                let mut reader = Reader::from(component);
                let _ = component_registry
                    .raw_write(
                        &mut reader,
                        &mut local_entity_mut,
                        remote_tick,
                        &mut remote_entity_map.remote_to_local,
                        events,
                    )
                    .inspect_err(|e| {
                        error!("could not write the component to the entity: {:?}", e)
                    });
            }
        }
        self.update_confirmed_tick(world, group_id, remote_tick, remote_entity_map);
    }

    pub(crate) fn apply_updates_message(
        &mut self,
        world: &mut World,
        remote: Option<ClientId>,
        component_registry: &ComponentRegistry,
        remote_tick: Tick,
        is_history: bool,
        message: EntityUpdatesMessage,
        events: &mut ConnectionEvents,
        remote_entity_map: &mut RemoteEntityMap,
    ) {
        let group_id = message.group_id;
        debug!(?remote_tick, ?message, "Received replication updates");
        // TODO: store this in ConfirmedHistory?
        if is_history {
            return;
        }
        for (entity, components) in message.updates.into_iter() {
            debug!(?components, remote_entity = ?entity, "Received UpdateComponent");
            // update the entity only if it exists
            if let Some(mut local_entity_mut) = remote_entity_map.get_by_remote(world, entity) {
                for component in components {
                    let mut reader = Reader::from(component);
                    let _ = component_registry
                        .raw_write(
                            &mut reader,
                            &mut local_entity_mut,
                            remote_tick,
                            &mut remote_entity_map.remote_to_local,
                            events,
                        )
                        .inspect_err(|e| {
                            error!("could not write the component to the entity: {:?}", e)
                        });
                }
            } else {
                // we can get a few buffered updates after the entity has been despawned
                // those are the updates that we received before the despawn action message, but with a tick
                // later than the despawn action message
                debug!("update for entity that doesn't exist?");
            }
        }
        self.update_confirmed_tick(world, group_id, remote_tick, remote_entity_map);
    }

    /// Update the Confirmed tick for all entities in the replication group
    /// so that Predicted/Interpolated entities can be notified
    ///
    /// We update it for all entities in the group (even if we received only an update that contains
    /// updates for E1, it also means that E2 is updated to the same tick, since they are part of the
    /// same group)
    pub(crate) fn update_confirmed_tick(
        &mut self,
        world: &mut World,
        group_id: ReplicationGroupId,
        remote_tick: Tick,
        remote_entity_map: &mut RemoteEntityMap,
    ) {
        // TODO: maybe get the confirmed tick from the apply_world message directly?
        // // let confirmed_tick = self.group_channels.get(&group_id).unwrap().latest_tick;
        // if let Some(group_channel) = self.group_channels
        //     .get(&group_id) {
        //     grou.remote_entities
        //
        // }

        self.remote_entities.iter().for_each(|remote_entity| {
            if let Some(mut local_entity_mut) =
                remote_entity_map.get_by_remote(world, *remote_entity)
            {
                trace!(?remote_tick, "updating confirmed tick for entity");
                if let Some(mut confirmed) = local_entity_mut.get_mut::<Confirmed>() {
                    confirmed.tick = remote_tick;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::replication::EntityActions;

    /// Test that the UpdatesIterator works correctly, when we want to iterate through
    /// the buffered updates we have received
    #[test]
    fn test_read_update_messages() {
        let mut manager = ReplicationReceiver::new();
        let group_id = ReplicationGroupId(0);

        manager
            .group_channels
            .entry(group_id)
            .or_default()
            .latest_tick = Some(Tick(1));
        // not even inserted because in the past compared to what we have applied
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id,
                last_action_tick: Some(Tick(0)),
                updates: Default::default(),
            },
            Tick(0),
        );
        // insert some updates
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id,
                last_action_tick: Some(Tick(1)),
                updates: Default::default(),
            },
            Tick(2),
        );
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id,
                last_action_tick: Some(Tick(3)),
                updates: Default::default(),
            },
            Tick(5),
        );
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id,
                last_action_tick: Some(Tick(6)),
                updates: Default::default(),
            },
            Tick(10),
        );
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id,
                last_action_tick: Some(Tick(6)),
                updates: Default::default(),
            },
            Tick(15),
        );

        assert_eq!(
            manager
                .group_channels
                .get(&group_id)
                .unwrap()
                .buffered_updates
                .len(),
            4
        );

        let mut it = manager
            .group_channels
            .get_mut(&group_id)
            .unwrap()
            .read_updates();
        assert_eq!(
            it.next().unwrap(),
            Update {
                remote_tick: Tick(2),
                message: EntityUpdatesMessage {
                    group_id,
                    last_action_tick: Some(Tick(1)),
                    updates: Default::default(),
                },
                is_history: false,
            }
        );
        assert!(it.next().is_none());
        assert_eq!(
            manager
                .group_channels
                .get(&group_id)
                .unwrap()
                .buffered_updates
                .len(),
            3
        );
        // we received a new action tick
        manager
            .group_channels
            .entry(group_id)
            .or_default()
            .latest_tick = Some(Tick(6));

        let mut it = manager
            .group_channels
            .get_mut(&group_id)
            .unwrap()
            .read_updates();
        assert_eq!(
            it.next().unwrap(),
            Update {
                remote_tick: Tick(5),
                message: EntityUpdatesMessage {
                    group_id,
                    last_action_tick: Some(Tick(3)),
                    updates: Default::default(),
                },
                is_history: true,
            }
        );
        assert_eq!(
            it.next().unwrap(),
            Update {
                remote_tick: Tick(10),
                message: EntityUpdatesMessage {
                    group_id,
                    last_action_tick: Some(Tick(6)),
                    updates: Default::default(),
                },
                is_history: true,
            }
        );
        assert_eq!(
            it.next().unwrap(),
            Update {
                remote_tick: Tick(15),
                message: EntityUpdatesMessage {
                    group_id,
                    last_action_tick: Some(Tick(6)),
                    updates: Default::default(),
                },
                is_history: false,
            }
        );
        assert!(it.next().is_none());
    }

    #[allow(clippy::get_first)]
    #[test]
    fn test_recv_replication_messages() {
        let mut manager = ReplicationReceiver::new();

        let group_id = ReplicationGroupId(0);
        // recv an actions message that is too old: should be ignored
        manager.recv_actions(
            EntityActionsMessage {
                group_id,
                sequence_id: MessageId(0) - 1,
                actions: Default::default(),
            },
            Tick(0),
        );
        assert_eq!(
            manager
                .group_channels
                .get(&group_id)
                .unwrap()
                .actions_pending_recv_message_id,
            MessageId(0)
        );
        assert!(manager
            .group_channels
            .get(&group_id)
            .unwrap()
            .actions_recv_message_buffer
            .is_empty());

        // recv an actions message: in order, should be buffered
        manager.recv_actions(
            EntityActionsMessage {
                group_id: ReplicationGroupId(0),
                sequence_id: MessageId(0),
                actions: Default::default(),
            },
            Tick(0),
        );
        assert!(manager
            .group_channels
            .get(&group_id)
            .unwrap()
            .actions_recv_message_buffer
            .contains_key(&MessageId(0)));

        // add an updates message
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id: ReplicationGroupId(0),
                last_action_tick: Some(Tick(0)),
                updates: Default::default(),
            },
            Tick(1),
        );
        assert_eq!(
            manager
                .group_channels
                .get(&group_id)
                .unwrap()
                .buffered_updates
                .0,
            vec![(
                Tick(1),
                EntityUpdatesMessage {
                    group_id: ReplicationGroupId(0),
                    last_action_tick: Some(Tick(0)),
                    updates: Default::default(),
                }
            )]
        );

        // add updates before actions (last_action_tick is 2)
        manager.recv_updates(
            EntityUpdatesMessage {
                group_id: ReplicationGroupId(0),
                last_action_tick: Some(Tick(3)),
                updates: Default::default(),
            },
            Tick(5),
        );
        assert_eq!(
            manager
                .group_channels
                .get(&group_id)
                .unwrap()
                .buffered_updates
                .0,
            vec![
                (
                    Tick(5),
                    EntityUpdatesMessage {
                        group_id: ReplicationGroupId(0),
                        last_action_tick: Some(Tick(3)),
                        updates: Default::default(),
                    }
                ),
                (
                    Tick(1),
                    EntityUpdatesMessage {
                        group_id: ReplicationGroupId(0),
                        last_action_tick: Some(Tick(0)),
                        updates: Default::default(),
                    }
                )
            ]
        );

        // read messages: only read the first action and update
        {
            let mut actions = manager.read_actions(Tick(10));
            let (tick, _) = actions.next().unwrap();
            assert_eq!(tick, Tick(0));
            assert!(actions.next().is_none());
        }
        {
            let mut updates = manager.read_updates();
            let update = updates.next().unwrap();
            assert_eq!(update.remote_tick, Tick(1));
            assert!(updates.next().is_none());
        }

        // recv actions-3: should be buffered, we are still waiting for actions-2
        manager.recv_actions(
            EntityActionsMessage {
                group_id: ReplicationGroupId(0),
                sequence_id: MessageId(2),
                actions: Default::default(),
            },
            Tick(3),
        );
        // if we tried to iterate actions, we get nothing because we are still waiting for actions-2
        assert!(manager.read_actions(Tick(2)).next().is_none());
        // recv actions-2: we should now be able to read actions-2, actions-3, updates-4
        manager.recv_actions(
            EntityActionsMessage {
                group_id: ReplicationGroupId(0),
                sequence_id: MessageId(1),
                actions: Default::default(),
            },
            Tick(2),
        );
        {
            let mut actions = manager.read_actions(Tick(10));
            let (tick, _) = actions.next().unwrap();
            assert_eq!(tick, Tick(2));
            let (tick, _) = actions.next().unwrap();
            assert_eq!(tick, Tick(3));
            assert!(actions.next().is_none());
        }
        let mut updates = manager.read_updates();
        let update = updates.next().unwrap();
        assert_eq!(update.remote_tick, Tick(5));
        assert!(!update.is_history);
        assert!(updates.next().is_none());
    }

    /// Test applying to the world an EntityActionsMessage that uses SpawnReuse
    #[test]
    fn test_recv_spawn_reuse() {
        let mut manager = ReplicationReceiver::new();
        let mut world = World::new();
        let remote_entity = Entity::from_raw(1000);
        let local_entity = world.spawn_empty().id();
        let component_registry = ComponentRegistry::default();
        let mut events = ConnectionEvents::default();
        let replication = EntityActionsMessage {
            group_id: ReplicationGroupId(0),
            sequence_id: MessageId(0),
            actions: vec![(
                remote_entity,
                EntityActions {
                    spawn: SpawnAction::Reuse(local_entity),
                    insert: vec![],
                    remove: Default::default(),
                    updates: vec![],
                },
            )],
        };
        manager.apply_actions_message(
            &mut world,
            None,
            &component_registry,
            Tick(0),
            replication,
            &mut events,
        );

        // check that no new entities were spawned
        assert_eq!(world.entities().len(), 1);
        // check that the entity mapping was updated
        assert_eq!(
            manager.remote_entity_map.get_local(remote_entity).unwrap(),
            &local_entity
        );
    }
}
