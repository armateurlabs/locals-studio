import type { JsonObject, Profile, Snapshot } from './types';
import LibraryView from './views/LibraryView';

interface Props {
  snapshot: Snapshot; profile: Profile; onClose: () => void;
  onAction: (method: string, params?: JsonObject) => Promise<any>;
  onUseInChat: (profile: Profile) => void;
}

export default function ModelDetails({ snapshot, profile, onClose, onAction, onUseInChat }: Props) {
  return <LibraryView key={profile.id} detailsOnly detailsProfile={profile} onCloseDetails={onClose} snapshot={snapshot} onAction={onAction} onUseInChat={onUseInChat} onSearch={() => {}} />;
}
