import native from 'liblightcraft.so';
import { AbilityConstant, Configuration, UIAbility, Want } from '@kit.AbilityKit';
import { window } from '@kit.ArkUI';
import { BusinessError } from '@kit.BasicServicesKit';
import { publishSystemTheme, systemThemeFromColorMode } from '../SystemTheme';

class SystemThemeInput {
  kind: string = 'system_theme';
  text: string = '';
  theme: string = '';
  constructor(token: string) {
    this.theme = token;
  }
}

export default class EntryAbility extends UIAbility {
  private syncSystemTheme(colorMode: number | undefined): void {
    let theme = systemThemeFromColorMode(colorMode);
    publishSystemTheme(theme);
    // Push live light/dark changes into the native worker; init carries the first value too.
    try {
      native.input(JSON.stringify(new SystemThemeInput(theme)));
    } catch (error) {
      console.info('System theme input unavailable before native init');
    }
  }

  onCreate(_want: Want, _launchParam: AbilityConstant.LaunchParam): void {
    this.syncSystemTheme(this.context.config?.colorMode);
    console.info('LightCraft native ability created');
  }

  onConfigurationUpdate(newConfig: Configuration): void {
    this.syncSystemTheme(newConfig.colorMode);
  }

  onForeground(): void {
    // The window may change appearance while backgrounded; re-read and wake the render worker.
    this.syncSystemTheme(this.context.config?.colorMode);
    native.active(true);
  }

  onBackground(): void {
    try {
      native.active(false);
    } catch (error) {
      console.info('LightCraft active(false) unavailable');
    }
  }

  onWindowStageCreate(windowStage: window.WindowStage): void {
    windowStage.on('windowStageClose', () => {
      try {
        native.close();
      } catch (error) {
        console.info('LightCraft close unavailable');
      }
    });
    windowStage.loadContent('pages/Index', (error: BusinessError) => {
      if (error.code) {
        console.error(`LightCraft page failed: ${error.code} ${error.message}`);
        return;
      }
      // Colour the status bar / navigation indicator to match the editor's dark chrome.
      try {
        let win = windowStage.getMainWindowSync();
        win.setWindowLayoutFullScreen(true);
      } catch (err) {
        let failure = err as BusinessError;
        console.error(`System bars failed: ${failure.code} ${failure.message}`);
      }
    });
  }

  onWindowStageDestroy(): void {
    console.info('LightCraft window stage destroyed');
  }
}
