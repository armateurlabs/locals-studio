export const FAVORITE_MODELS_KEY = 'studio.library.favoriteModelIds';

export function loadFavoriteModels(): Set<string> {
  try {
    const saved: unknown = JSON.parse(localStorage.getItem(FAVORITE_MODELS_KEY) ?? '[]');
    return new Set(Array.isArray(saved) ? saved.filter((id): id is string => typeof id === 'string' && id.length > 0) : []);
  } catch { return new Set(); }
}
