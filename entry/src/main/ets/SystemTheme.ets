// Maps HarmonyOS Configuration.colorMode to a LightCraft appearance token and caches the
// last known value so the page can send it to the native host at startup.
import { Configuration } from '@kit.AbilityKit';

export const SYSTEM_THEME_LIGHT: string = 'light';
export const SYSTEM_THEME_DARK: string = 'dark';

export function systemThemeFromColorMode(colorMode: number | undefined): string {
  if (colorMode === undefined) {
    return '';
  }
  // ConfigurationConstant.ColorMode: 0 = light, 1 = dark, 2 = follow system.
  if (colorMode === 1) {
    return SYSTEM_THEME_DARK;
  }
  if (colorMode === 0) {
    return SYSTEM_THEME_LIGHT;
  }
  return '';
}

export function publishSystemTheme(token: string): void {
  AppStorage.setOrCreate('lightcraftSystemTheme', token);
}

export function readSystemTheme(): string {
  let token = AppStorage.get<string>('lightcraftSystemTheme') ?? '';
  if (token !== SYSTEM_THEME_DARK && token !== SYSTEM_THEME_LIGHT) {
    return '';
  }
  return token;
}

export function themeFromConfiguration(config: Configuration | undefined): string {
  if (config === undefined) {
    return '';
  }
  return systemThemeFromColorMode(config.colorMode);
}
