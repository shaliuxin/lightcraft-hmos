import native from 'liblightcraft.so';
import { common } from '@kit.AbilityKit';
import { fileIo, fileUri, picker } from '@kit.CoreFileKit';
import { i18n } from '@kit.LocalizationKit';
import { BusinessError, deviceInfo } from '@kit.BasicServicesKit';
import { display, KeyboardAvoidMode } from '@kit.ArkUI';
import { readSystemTheme } from '../SystemTheme';

// Matches the Config::from_json contract in the Rust worker (native/rust/src/lib.rs).
class NativeConfig {
  files: string = '';
  documents: string = '';
  scale: number = 1;
  language: string = '';
  systemTheme: string = '';
  deviceType: string = '';
  widthVp: number = 0;
  constructor(files: string, documents: string, scale: number, language: string,
              systemTheme: string, deviceType: string, widthVp: number) {
    this.files = files;
    this.documents = documents;
    this.scale = scale;
    this.language = language;
    this.systemTheme = systemTheme;
    this.deviceType = deviceType;
    this.widthVp = widthVp;
  }
}

class NativeMessage {
  kind: string = '';
  text: string = '';
}

class NativeInput {
  kind: string = '';
  code: number = 0;
  action: number = 0;
}

function basename(name: string): string {
  if (name.length === 0) {
    return '';
  }
  let idx = name.lastIndexOf('/');
  if (idx < 0) {
    idx = name.lastIndexOf('\\');
  }
  return idx >= 0 ? name.substring(idx + 1) : name;
}

@Entry
@Component
struct Index {
  @State private failure: string = '';
  @State private initialized: boolean = false;
  @State private startupVisible: boolean = true;
  @State private startupOpacity: number = 1;
  @State private startupScale: number = 1;
  private startupFading: boolean = false;
  private context: common.UIAbilityContext = this.getUIContext().getHostContext() as common.UIAbilityContext;
  private generation: number = 0;
  // A recreated page must not let a stale async result drive the current page.
  private alive: boolean = false;
  // The URI a save-picker granted us, kept so an ordinary write can reuse it.
  private exportUri: string = '';

  aboutToAppear(): void {
    console.info('LightCraft native page appearing');
    this.alive = true;
    this.generation++;
    this.startupVisible = true;
    this.startupOpacity = 1;
    this.startupScale = 1;
    this.startupFading = false;
    this.getUIContext().setKeyboardAvoidMode(KeyboardAvoidMode.RESIZE);
    this.initializeHost(this.generation);
  }

  private async initializeHost(generation: number): Promise<void> {
    try {
      // Presets and export defaults live in the app sandbox (the engine creates subdirs on demand).
      let documents = this.context.filesDir + '/Documents';
      if (!this.alive || generation !== this.generation) {
        return;
      }
      let scale: number = this.getUIContext().vp2px(1);
      let widthVp: number = this.getUIContext().px2vp(display.getDefaultDisplaySync().width);
      let language = i18n.System.getSystemLanguage();
      let systemTheme = readSystemTheme();
      let config = new NativeConfig(this.context.filesDir, documents, scale, language, systemTheme,
        deviceInfo.deviceType, widthVp);
      native.init(JSON.stringify(config), (id: number, message: string, data: ArrayBuffer): void => {
        this.receive(id, message, data);
      });
      this.initialized = true;
      this.failure = '';
    } catch (error) {
      if (this.alive && generation === this.generation) {
        this.failure = (error as Error).message;
      }
    }
  }

  aboutToDisappear(): void {
    this.alive = false;
    this.generation++;
    this.initialized = false;
    try {
      native.active(false);
    } catch (error) {
      console.info('LightCraft active(false) unavailable');
    }
  }

  onBackPress(): boolean {
    // Map the system back gesture to an Escape key-down/up for the editor.
    let input = new NativeInput();
    input.kind = 'key';
    input.code = 2051;
    input.action = 0;
    native.input(JSON.stringify(input));
    input.action = 1;
    native.input(JSON.stringify(input));
    return true;
  }

  private receive(id: number, message: string, data: ArrayBuffer): void {
    let request = JSON.parse(message) as NativeMessage;
    if (request.kind !== 'ready') {
      console.info(`LightCraft native ${request.kind}: ${request.text}`);
    }
    if (!this.alive) {
      if (id > 0) {
        native.reply(id, false, 'cancelled', new ArrayBuffer(0));
      }
      return;
    }
    if (request.kind === 'error') {
      this.failure = request.text;
    } else if (request.kind === 'ready') {
      this.failure = '';
      this.revealEditor();
    } else if (request.kind === 'url') {
      this.context.startAbility({ action: 'ohos.want.action.viewData', uri: request.text })
        .catch((error: BusinessError) => {
          console.error(`External link: ${error.message}`);
        });
    } else if (request.kind === 'close') {
      this.context.terminateSelf();
    } else if (request.kind === 'open') {
      this.openPhotos(id);
    } else if (request.kind === 'open_library') {
      this.openLibrary(id);
    } else if (request.kind === 'export') {
      this.exportPhoto(id, request.text, data);
    } else if (request.kind === 'save_preset') {
      this.savePreset(id, request.text);
    }
  }

  private revealEditor(): void {
    if (!this.startupVisible || this.startupFading) {
      return;
    }
    this.startupFading = true;
    const generation = this.generation;
    this.getUIContext().animateTo({
      duration: 320,
      curve: Curve.EaseOut,
      onFinish: (): void => {
        if (!this.alive || generation !== this.generation) {
          return;
        }
        this.startupVisible = false;
        this.restoreFocus();
      }
    }, (): void => {
      this.startupOpacity = 0;
      this.startupScale = 0.98;
    });
  }

  private restoreFocus(): void {
    if (this.alive && this.initialized) {
      this.getUIContext().getFocusController().requestFocus('lightcraft-editor');
    }
  }

  private async openPhotos(id: number): Promise<void> {
    let generation = this.generation;
    try {
      // The document picker covers photos and camera-RAW alike (the gallery picker hides RAW).
      let uris = await this.selectDocumentUris();
      if (!this.alive || generation !== this.generation || uris.length === 0) {
        native.reply(id, false, 'cancelled', new ArrayBuffer(0));
        return;
      }
      let paths: string[] = [];
      for (let uri of uris) {
        let p = new fileUri.FileUri(uri).path;
        if (p.length > 0) {
          paths.push(p);
        }
      }
      native.reply(id, true, JSON.stringify(paths), new ArrayBuffer(0));
      this.restoreFocus();
    } catch (error) {
      native.reply(id, false, (error as Error).message, new ArrayBuffer(0));
      this.restoreFocus();
    }
  }

  private async openLibrary(id: number): Promise<void> {
    let generation = this.generation;
    try {
      let options = new picker.DocumentSelectOptions();
      options.selectMode = picker.DocumentSelectMode.FOLDER;
      let uris = await new picker.DocumentViewPicker(this.context).select(options);
      if (!this.alive || generation !== this.generation || uris.length === 0) {
        native.reply(id, false, 'cancelled', new ArrayBuffer(0));
        return;
      }
      let path = new fileUri.FileUri(uris[0]).path;
      native.reply(id, true, path, new ArrayBuffer(0));
      this.restoreFocus();
    } catch (error) {
      native.reply(id, false, (error as Error).message, new ArrayBuffer(0));
      this.restoreFocus();
    }
  }

  private async exportPhoto(id: number, suggested: string, bytes: ArrayBuffer): Promise<void> {
    let generation = this.generation;
    try {
      let name = basename(suggested);
      let options = new picker.DocumentSaveOptions();
      options.newFileNames = name.length > 0 ? [name] : undefined;
      let uris = await new picker.DocumentViewPicker(this.context).save(options);
      if (!this.alive || generation !== this.generation || uris.length === 0) {
        native.reply(id, false, 'cancelled', new ArrayBuffer(0));
        return;
      }
      this.exportUri = uris[0];
      await this.writeBytes(uris[0], bytes);
      native.reply(id, true, basename(suggested), new ArrayBuffer(0));
      this.restoreFocus();
    } catch (error) {
      native.reply(id, false, (error as Error).message, new ArrayBuffer(0));
      this.restoreFocus();
    }
  }

  private async savePreset(id: number, suggested: string): Promise<void> {
    let generation = this.generation;
    try {
      let name = basename(suggested).length > 0 ? basename(suggested) : 'preset.lcpreset';
      let options = new picker.DocumentSaveOptions();
      options.newFileNames = [name];
      let uris = await new picker.DocumentViewPicker(this.context).save(options);
      if (!this.alive || generation !== this.generation || uris.length === 0) {
        native.reply(id, false, 'cancelled', new ArrayBuffer(0));
        return;
      }
      let path = new fileUri.FileUri(uris[0]).path;
      native.reply(id, true, path, new ArrayBuffer(0));
      this.restoreFocus();
    } catch (error) {
      native.reply(id, false, (error as Error).message, new ArrayBuffer(0));
      this.restoreFocus();
    }
  }

  private async writeBytes(uri: string, bytes: ArrayBuffer): Promise<void> {
    let file = await fileIo.open(uri, fileIo.OpenMode.READ_WRITE | fileIo.OpenMode.CREATE | fileIo.OpenMode.TRUNC);
    try {
      await fileIo.truncate(file.fd, 0);
      let offset = 0;
      while (offset < bytes.byteLength) {
        let chunk = bytes.slice(offset, Math.min(offset + 1024 * 1024, bytes.byteLength));
        let written = await fileIo.write(file.fd, chunk);
        if (written <= 0) {
          throw new Error('incomplete file write');
        }
        offset += written;
      }
      await fileIo.fsync(file.fd);
    } finally {
      await fileIo.close(file);
    }
  }

  private async selectDocumentUris(): Promise<Array<string>> {
    let options = new picker.DocumentSelectOptions();
    options.maxSelectNumber = 1;
    return await new picker.DocumentViewPicker(this.context).select(options);
  }

  build() {
    Stack() {
      if (this.initialized) {
        XComponent({ id: 'lightcraft-native', type: XComponentType.SURFACE, libraryname: 'lightcraft' })
          .width('100%')
          .height('100%')
          .onLoad((): void => {
            this.restoreFocus();
          })
          .id('lightcraft-editor')
          .focusable(true)
          .defaultFocus(true)
          .onBlur((): void => {
            let input = new NativeInput();
            input.kind = 'blur';
            native.input(JSON.stringify(input));
          })
      }
      if (this.startupVisible) {
        Column() {
          Column({ space: 18 }) {
            Image($r('app.media.app_icon')).width(76).height(76).borderRadius(18)
            Text('LightCraft').fontSize(30).fontWeight(FontWeight.Medium).fontColor('#F4F4F5')
            Row({ space: 10 }) {
              LoadingProgress().width(18).height(18).color('#A5BFFF')
              Text(this.initialized ? '正在启动编辑器…' : '正在准备运行环境…')
                .fontSize(14).fontColor('#B8BAC2')
            }
          }.scale({ x: this.startupScale, y: this.startupScale })
        }
        .width('100%')
        .height('100%')
        .justifyContent(FlexAlign.Center)
        .backgroundColor('#1E1E1E')
        .opacity(this.startupOpacity)
        .hitTestBehavior(HitTestMode.Block)
        .accessibilityText('LightCraft 正在启动')
      }
      if (this.failure.length > 0) {
        Column() {
          Text('LightCraft 原生运行错误').fontSize(20).fontColor('#FFFFFF')
          Text(this.failure).fontSize(14).fontColor('#FFFFFF').margin({ top: 12 })
          if (!this.initialized) {
            Button('重试').onClick((): void => {
              this.failure = '';
              this.initializeHost(this.generation);
            })
          } else {
            Button('关闭提示').onClick((): void => {
              this.failure = '';
            })
          }
        }.padding(24).backgroundColor('#DD262626').borderRadius(8).width('80%')
      }
    }.width('100%').height('100%').backgroundColor('#1E1E1E')
  }
}
