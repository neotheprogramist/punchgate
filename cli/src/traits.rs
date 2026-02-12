/// A Mealy machine coalgebra: `(State, Event) → (State, Vec<Command>)`.
///
/// Both sub-machines (`PeerState`, `TunnelState`) and the composed
/// `AppState` implement this trait with the same associated types,
/// enabling independent testing and monadic composition.
pub trait MealyMachine: Sized {
    type Event;
    type Command;
    fn transition(self, event: Self::Event) -> (Self, Vec<Self::Command>);
}
